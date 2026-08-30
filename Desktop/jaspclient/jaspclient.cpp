#include "jaspclient.h"

#include <nng/protocol/pair1/pair.h>
#include <nng/protocol/reqrep0/req.h>

#include <QMetaObject>
#include <QMetaType>
#include <QLocale>
#include <QtGlobal>

#include <chrono>
#include <cstring>
#include <memory>
#include <mutex>

#include "log.h"

namespace
{
// Handshake / retry timing. Every handshake step is bounded so a missing or unresponsive
// orchestrator fails fast and the connection loop retries, rather than blocking the worker thread.
constexpr nng_duration kHandshakeTimeoutMs = 3000;	// REQ send/recv timeout
constexpr int          kReconnectChunks    = 10;	// back off ~1s between attempts ...
constexpr int          kChunkMs            = 100;	// ... in 100ms chunks so shutdown stays prompt
constexpr int          kPeerBufDepth       = 64;	// PAIR SENDBUF/RECVBUF (messages)
// §18.4/§4.4 wire ceiling: libnng's RecvMaxSize DEFAULTS TO ~1 MiB AND SILENTLY DISCARDS
// larger messages (neo-jasp §25.5). View chunks (~20 MB) and any future bulk ride this
// channel, so raise it to the shared ceiling + envelope margin — matching the orchestrator
// and the data lane. nanonext/R raises it internally; the C endpoint must do it itself.
constexpr size_t kRecvMaxSize = (size_t(256) + 16) * 1024 * 1024;
}

JaspClient * JaspClient::_singleton = nullptr;

JaspClient * JaspClient::client() { return _singleton; }

JaspClient::JaspClient(QObject * parent)
	: QObject(parent)
{
	_singleton = this;

	// Catalog entries cross the worker→GUI hop and may cross thread boundaries via the signal.
	qRegisterMetaType<ModuleCatalog>("ModuleCatalog");

	// Wire-IO log level, opt-in via env: JASP_CLIENT_LOG=off|stub|full (default off).
	const QByteArray logLevel = qgetenv("JASP_CLIENT_LOG").toLower();
	_verbosity = (logLevel == "full") ? Verbosity::Full
			   : (logLevel == "stub") ? Verbosity::Stub
			   :						  Verbosity::Off;
}

JaspClient::~JaspClient()
{
	_running = false;
	{
		std::lock_guard<std::mutex> lock(_socketMutex);
		if (_socketOpen)
		{
			nng_close(_socket);					// unblocks the receiver thread
			_socketOpen = false;
		}
	}
	if (_recvThread.joinable())
		_recvThread.join();
	if (_singleton == this)
		_singleton = nullptr;
}

void JaspClient::connectToOrchestrator(const std::string & url)
{
	if (_running)
		return;									// already connecting / connected

	_url		= url;
	_running	= true;
	_recvThread = std::thread(&JaspClient::connectionLoop, this);
}

// ── connection lifecycle (single worker thread) ─────────────────────────────

void JaspClient::connectionLoop()
{
	while (_running)
	{
		if (handshake() && openDataChannel())
		{
			_connected = true;
			recvLoop();							// blocks until the channel drops or we shut down
			_connected = false;
			closeDataChannel();
		}
		// Back off before (re)handshaking, in small chunks so shutdown stays prompt.
		for (int i = 0; i < kReconnectChunks && _running; ++i)
			std::this_thread::sleep_for(std::chrono::milliseconds(kChunkMs));
	}
}

bool JaspClient::handshake()
{
	nng_socket req;
	int rv = nng_req0_open(&req);
	if (rv != 0)
	{
		Log::log() << "JaspClient: nng_req0_open failed: " << nng_strerror(rv) << std::endl;
		return false;
	}
	// Bound every step so a missing/unresponsive orchestrator fails fast and we retry.
	nng_socket_set_ms(req, NNG_OPT_SENDTIMEO, kHandshakeTimeoutMs);
	nng_socket_set_ms(req, NNG_OPT_RECVTIMEO, kHandshakeTimeoutMs);
	// Harmless on the handshake REQ (its replies are small), but uniform with the data channel.
	nng_socket_set_size(req, NNG_OPT_RECVMAXSZ, kRecvMaxSize);

	// Non-blocking dial: NNG connects in the background; the send below times out if it cannot.
	rv = nng_dial(req, _url.c_str(), nullptr, NNG_FLAG_NONBLOCK);
	if (rv != 0)
	{
		Log::log() << "JaspClient: dial " << _url << " (" << nng_strerror(rv) << "); will retry." << std::endl;
		nng_close(req);
		return false;
	}

	Json::Value hello(Json::objectValue);
	hello["v"]		= 1;
	hello["id"]		= "hello-" + std::to_string(_nextHello++);
	hello["type"]	= "hello";
	// client_id / client_version are optional reconnect hints (deferred); omitted for now.

	QByteArray reqFrame = frameEnvelope(hello);
	rv = nng_send(req, reqFrame.data(), static_cast<size_t>(reqFrame.size()), 0);
	if (rv != 0)
	{
		Log::log() << "JaspClient: hello send failed: " << nng_strerror(rv) << std::endl;
		nng_close(req);
		return false;
	}

	nng_msg * msg = nullptr;
	rv = nng_recvmsg(req, &msg, 0);
	if (rv != 0)
	{
		Log::log() << "JaspClient: welcome recv failed: " << nng_strerror(rv) << std::endl;
		nng_close(req);
		return false;
	}
	const QByteArray body(static_cast<const char *>(nng_msg_body(msg)), static_cast<int>(nng_msg_len(msg)));
	nng_msg_free(msg);
	nng_close(req);							// the control connection is transient: one request -> reply

	const Json::Value welcome = deframeEnvelope(body);
	if (welcome.isNull() || welcome.get("type", "").asString() != "welcome")
	{
		Log::log() << "JaspClient: expected welcome on control endpoint; retrying." << std::endl;
		return false;
	}
	if (!welcome.get("ok", false).asBool())
	{
		Log::log() << "JaspClient: hello rejected: " << welcome.get("error", "?").asString() << std::endl;
		return false;
	}
	_channelUrl = welcome.get("channel_url", "").asString();
	_sessionId  = welcome.get("session_id", "").asString();	// orchestrator-assigned, on the envelope
	if (_channelUrl.empty())
	{
		Log::log() << "JaspClient: welcome missing channel_url" << std::endl;
		return false;
	}

	Log::log() << "JaspClient: handshake ok — session " << _sessionId << ", data channel " << _channelUrl << std::endl;
	return true;
}

bool JaspClient::openDataChannel()
{
	std::lock_guard<std::mutex> lock(_socketMutex);

	nng_socket s;
	int rv = nng_pair1_open(&s);
	if (rv != 0)
	{
		Log::log() << "JaspClient: nng_pair1_open failed: " << nng_strerror(rv) << std::endl;
		return false;
	}
	// Peer-side buffers: the orchestrator sends with non-blocking try_send, so it needs us to have
	// recv room for its frames to land even before we post the next recv.
	nng_socket_set_int(s, NNG_OPT_SENDBUF, kPeerBufDepth);
	nng_socket_set_int(s, NNG_OPT_RECVBUF, kPeerBufDepth);
	// §18.4/§4.4: raise the recv ceiling — the libnng default (~1 MiB) silently discards
	// larger messages, and view chunks (~20 MB) arrive on this channel.
	if (nng_socket_set_size(s, NNG_OPT_RECVMAXSZ, kRecvMaxSize) != 0)
		Log::log() << "JaspClient: could not raise NNG_OPT_RECVMAXSZ; large frames may be dropped." << std::endl;

	rv = nng_dial(s, _channelUrl.c_str(), nullptr, 0);	// blocking: the orchestrator just created this listener
	if (rv != 0)
	{
		Log::log() << "JaspClient: dial channel " << _channelUrl << " failed: " << nng_strerror(rv) << std::endl;
		nng_close(s);
		return false;
	}

	// Liveness: a dialed PAIR does not error its recv when the listener dies, and the dialer
	// silently retries the (now stale) channel URL forever — so without this, a restarted
	// orchestrator would strand us on a dead channel. The pipe-removal callback flags the drop;
	// recvLoop runs on a bounded recv timeout, notices, and exits so connectionLoop re-handshakes
	// and gets a fresh channel URL. (Same mechanism as the orchestrator's own pipe_notify.)
	nng_socket_set_ms(s, NNG_OPT_RECVTIMEO, 1000);
	if (nng_pipe_notify(s, NNG_PIPE_EV_REM_POST, &JaspClient::pipeEventCb, this) != 0)
		Log::log() << "JaspClient: nng_pipe_notify failed; reconnect on orchestrator restart degraded." << std::endl;
	_channelDropped = false;

	_socket		= s;
	_socketOpen = true;
	return true;
}

void JaspClient::pipeEventCb(nng_pipe, nng_pipe_ev ev, void * arg)
{
	if (ev == NNG_PIPE_EV_REM_POST)
		static_cast<JaspClient *>(arg)->_channelDropped = true;	// recvLoop (bounded recv) notices and exits
}

void JaspClient::closeDataChannel()
{
	std::lock_guard<std::mutex> lock(_socketMutex);
	if (_socketOpen)
	{
		nng_close(_socket);
		_socketOpen = false;
	}
}

void JaspClient::recvLoop()
{
	while (_running)
	{
		nng_msg * msg = nullptr;
		int rv = nng_recvmsg(_socket, &msg, 0);		// bounded by RECVTIMEO (set in openDataChannel)
		if (rv != 0)
		{
			if (rv == NNG_ETIMEDOUT)
			{	// Quiet window — normally nothing; if the pipe died, re-handshake.
				if (_channelDropped)
				{
					Log::log() << "JaspClient: data channel pipe removed; re-handshaking." << std::endl;
					return;							// connectionLoop re-handshakes (fresh channel URL)
				}
				continue;
			}
			if (!_running)
				break;								// socket closed during shutdown
			Log::log() << "JaspClient: data channel recv error (" << nng_strerror(rv) << "); re-handshaking." << std::endl;
			return;									// pipe dropped -> connectionLoop re-handshakes
		}

		QByteArray body(static_cast<const char *>(nng_msg_body(msg)), static_cast<int>(nng_msg_len(msg)));
		nng_msg_free(msg);

		// The single thread-hop: parse + dispatch happen on the GUI thread.
		QMetaObject::invokeMethod(this, [this, body]() { handleMessage(body); }, Qt::QueuedConnection);
	}
}

// ── public API (main thread) ─────────────────────────────────────────────────

std::string JaspClient::submit(const Json::Value & work, ResultHandler handler, const QByteArray & binary)
{
	std::string workId = work.get("work_id", "").asString();
	if (workId.empty())
		workId = "w" + std::to_string(_nextWork++);	// generic one-shot work (analyses supply their id)

	const uint64_t revision = work.get("revision", 0).asUInt64();

	// Same work_id re-submitted → the assignment replaces the old slot (eviction by construction).
	// No abort dance and no separate per-analysis map: this revision is the high-water mark (§23).
	_slots[workId] = Slot{ revision, work.get("kind", "analysis_r_classic_jaspbase").asString(), std::move(handler) };

	sendFrame(work, binary);
	return workId;
}

std::string JaspClient::submitDataEdit(const QString & datasetId, uint64_t baseRevision, const Json::Value & editOp, const QByteArray & tail, ResultHandler handler)
{
	// The wire shape mirrors ViewFiller's data_view envelope (one home for C++ data work) with
	// the edit family's payload: op data_edit + the adjacently-tagged edit object. `revision` is
	// the D11 echo — the caller reads DataSet::laneRevision() at submit time; the orchestrator
	// checks it against the dataset entry (stale → visible validationError, nothing applied).
	// The ingest rides empty except the locale separators: the lane parses pasted numbers under
	// them, everything else takes its defaults.
	Json::Value payload(Json::objectValue);
	payload["op"]			= "data_edit";
	payload["source"]		= "";		// orchestrator injects the pre-edit cache at dispatch
	payload["cache_path"]	= "";		// …and assigns the next revision's path
	payload["format"]		= "";		// edits are format-agnostic (they read the cache)
	Json::Value ingest(Json::objectValue);
	const QString decimal = QLocale::system().decimalPoint();
	if (decimal == QLatin1String(","))		// the lane default is '.'; only a comma-decimal locale overrides
		ingest["decimal_sep"]	= ",";
	payload["ingest"]		= ingest;
	payload["row_offset"]	= Json::UInt64(0);
	payload["row_limit"]		= Json::nullValue;
	payload["columns"]		= Json::nullValue;
	payload["max_bytes"]		= Json::UInt64(0);	// unused by edits (a view knob); explicit for the typed parse
	payload["render"]		= Json::nullValue;
	payload["edit"]			= editOp;

	Json::Value datasetIds(Json::arrayValue);
	datasetIds.append(datasetId.toStdString());

	Json::Value work(Json::objectValue);
	work["v"]			= 1;
	work["type"]		= "work";
	work["revision"]	= Json::UInt64(baseRevision);
	work["dataset_ids"]	= datasetIds;
	work["kind"]		= "data";
	work["payload"]	= payload;

	return submit(work, std::move(handler), tail);
}

void JaspClient::abort(const std::string & workId)
{
	_slots.erase(workId);

	Json::Value msg(Json::objectValue);
	msg["v"]		= 1;
	msg["type"]		= "abort";
	msg["id"]		= "abort-" + workId;
	msg["work_id"]	= workId;
	sendFrame(msg);
}

// ── wire IO ──────────────────────────────────────────────────────────────────────────────

void JaspClient::handleMessage(const QByteArray & body)
{
	// §18.1: envelope + optional binary tail. Bulk bytes (view chunks) never go through the
	// JSON parser — splitFrame hands them back verbatim.
	const auto [env, binary] = splitFrame(body);
	if (env.isNull())
		return;

	logIo("RX", env);

	const std::string type = env.get("type", "").asString();

	if (type == "data_changed")
	{	// Dataset revision bump (data-edit-design §6, Increment 4): one uniform
		// buffer-invalidation push for every revision bump, whatever caused it. It has no
		// `work_id` — the completion-slot routing does not apply; consumers route by
		// dataset_id (Workspace → DataSet::applyRevision). Rows is Option on the wire
		// ("always" in practice — the lane knows the total post-apply); schema is present
		// IFF it changed; the invalidation descriptor is always an object. `cause` is
		// diagnostics-only — it rides the envelope (visible in JASP_CLIENT_LOG), never the API.
		const Json::Value & rowsField = env["rows"];
		const bool hasRows = rowsField.isNumeric();
		emit dataChanged(QString::fromStdString(env.get("dataset_id", "").asString()),
						 env.get("dataset_revision", 0).asUInt64(),
						 hasRows ? rowsField.asUInt64() : 0,
						 hasRows,
						 env.get("schema", Json::nullValue),
						 env.get("invalidation", Json::Value(Json::objectValue)));
		return;
	}

	if (type == "modules")
	{	// Module catalog — connect-time push (first frame on the channel), change push, or a
		// list_modules reply: all identical to consumers (HANDOVER-client-discovery.md).
		_catalog = parseCatalog(env.get("modules", Json::nullValue));
		emit modulesUpdated(_catalog);
		return;
	}

	// Orchestrator error against a submitted work (e.g. dataset_not_ready): surfaced as a
	// fatalError result on that slot so the analysis does not spin forever. The slot remembers
	// the submitted work's kind, so the synthetic failure is shaped like its kind (§19.2).
	if (type == "error")
	{
		const std::string workId = env.get("work_id", "").asString();
		auto sit = workId.empty() ? _slots.end() : _slots.find(workId);
		if (sit != _slots.end())
		{
			ResultHandler handler = std::move(sit->second.handler);
			Result result;
			result.kind		= std::move(sit->second.kind);
			result.status	= "fatalError";
			result.message	= env.get("message", "").asString();
			_slots.erase(sit);
			if (result.kind == "analysis_r_classic_jaspbase")
			{	// The analysis UI surfaces failures from the results error tree.
				result.results					= Json::Value(Json::objectValue);
				result.results["error"]			= true;
				result.results["errorMessage"]	= result.message;
			}
			handler(result);
		}
		return;
	}

	if (type != "result")
		return;							// alpha only consumes results + the catalog

	const std::string workId	= env.get("work_id", "").asString();
	const uint64_t	revision	= env.get("revision", 0).asUInt64();
	const std::string status	= env.get("status", "complete").asString();
	const std::string kind		= env.get("kind", "").asString();
	const Json::Value payload	= env.get("payload", Json::nullValue);

	auto it = _slots.find(workId);
	if (it == _slots.end())
		return;								// unknown / already completed / aborted
	if (revision < it->second.revision)
		return;								// stale: superseded by a newer revision (§23)

	// Switch on kind; fill the kind-specific field group (§19.2 Result payloads by kind).
	Result result;
	result.kind		= kind;
	result.status	= status;
	if (kind == "analysis_r_classic_jaspbase")
	{
		result.results		= payload.get("results", Json::nullValue);
		result.resultsDir	= payload.get("results_dir", "").asString();
	}
	else if (kind == "data")
	{
		result.datasetId		= payload.get("dataset_id", "").asString();
		result.datasetRevision	= payload.get("dataset_revision", 0).asUInt64();
		result.rows				= payload.get("rows", 0).asUInt64();
		result.schema			= payload.get("schema", Json::nullValue);
		result.message			= payload.get("error_message", "").asString();
		// data_view chunk metadata + the TSV cells (§4.1; the tail rides the frame §18.1).
		result.rowOffset		= payload.get("row_offset", 0).asUInt64();
		result.rowCount			= payload.get("row_count", 0).asUInt64();
		result.truncated		= payload.get("truncated", false).asBool();
		result.binary			= binary;
		// data_edit additions (§5/D10): the inverse meta rides the payload; the IPC bytes ride the
		// frame tail (= `binary`). Stored verbatim by the undo command, never interpreted here.
		result.inverseMeta		= payload.get("inverse", Json::nullValue);
	}
	if (result.message.empty())
		result.message = env.get("message", "").asString();

	ResultHandler handler = it->second.handler;	// copy: the handler may erase/re-enter
	const bool terminal = (status == "complete" || status == "fatalError" || status == "validationError");
	if (terminal)
		_slots.erase(it);

	handler(result);
}

ModuleCatalog JaspClient::parseCatalog(const Json::Value & modulesJson)
{
	ModuleCatalog catalog;
	if (!modulesJson.isArray())
	{
		Log::log() << "JaspClient: `modules` is not an array; ignored." << std::endl;
		return catalog;
	}
	for (const Json::Value & entry : modulesJson)
	{
		const std::string name = entry.get("name", "").asString();
		if (name.empty())
		{
			Log::log() << "JaspClient: module entry without a name; skipped." << std::endl;
			continue;
		}
		// Tolerant of the rest: later schema additions (e.g. tie-break fields) are ignored for now.
		catalog.push_back(CatalogModule{ name, entry.get("version", "").asString(), entry.get("base_uri", "").asString() });
	}
	return catalog;
}

void JaspClient::sendFrame(const Json::Value & envelope, const QByteArray & binary)
{
	logIo("TX", envelope);

	std::lock_guard<std::mutex> lock(_socketMutex);
	if (!_socketOpen)
	{
		Log::log() << "JaspClient: sendFrame with no data channel; dropping." << std::endl;
		return;
	}

	QByteArray frame	= frameEnvelope(envelope, binary);
	const int		 rv		= nng_send(_socket, frame.data(), static_cast<size_t>(frame.size()), 0);
	if (rv != 0)
		Log::log() << "JaspClient: send failed: " << nng_strerror(rv) << std::endl;
}

// ── framing (§18.1) ──────────────────────────────────────────────

QByteArray JaspClient::frameEnvelope(const Json::Value & env, const QByteArray & binary)
{
	const std::string json	= env.toStyledString();
	const quint32		len	= static_cast<quint32>(json.size());

	QByteArray frame;
	frame.resize(static_cast<int>(4 + json.size() + binary.size()));
	char * d = frame.data();
	d[0] = static_cast<char>((len >> 24) & 0xff);
	d[1] = static_cast<char>((len >> 16) & 0xff);
	d[2] = static_cast<char>((len >> 8)  & 0xff);
	d[3] = static_cast<char>( len        & 0xff);
	std::memcpy(d + 4, json.data(), json.size());
	if (!binary.isEmpty())
		std::memcpy(d + 4 + json.size(), binary.constData(), size_t(binary.size()));
	return frame;
}

std::pair<Json::Value, QByteArray> JaspClient::splitFrame(const QByteArray & body)
{
	// Frame (§18.1): [u32 BE json_len][json][binary tail?]. The JSON envelope is parsed;
	// the trailing binary payload (e.g. a view chunk's escaped TSV) is returned verbatim —
	// bulk bytes never go through the JSON parser.
	if (body.size() < 4)
		return { Json::Value(), QByteArray() };

	const quint32 len = (static_cast<quint8>(body.at(0)) << 24) |
						(static_cast<quint8>(body.at(1)) << 16) |
						(static_cast<quint8>(body.at(2)) << 8)  |
						 static_cast<quint8>(body.at(3));

	if (static_cast<quint32>(body.size()) < 4u + len)
		return { Json::Value(), QByteArray() };

	Json::CharReaderBuilder rb;
	std::unique_ptr<Json::CharReader> reader(rb.newCharReader());
	Json::Value	env;
	std::string errs;
	const char * begin = body.constData() + 4;
	if (!reader->parse(begin, begin + len, &env, &errs))
	{
		Log::log() << "JaspClient: bad envelope JSON: " << errs << std::endl;
		return { Json::Value(), QByteArray() };
	}
	return { env, body.mid(static_cast<int>(4u + len)) };
}

Json::Value JaspClient::deframeEnvelope(const QByteArray & body)
{
	return splitFrame(body).first;
}

void JaspClient::logIo(const char * dir, const Json::Value & env) const
{
	if (_verbosity == Verbosity::Off)
		return;

	if (_verbosity == Verbosity::Full)
	{
		Log::log() << "JaspClient " << dir << ": " << env.toStyledString() << std::endl;
		return;
	}

	// Stub: a one-line summary of the message.
	std::string line = std::string("JaspClient ") + dir + ": " + env.get("type", "?").asString();

	const std::string workId = env.get("work_id", "").asString();
	if (!workId.empty())
		line += " work_id=" + workId;
	if (env.isMember("kind"))
		line += " kind=" + env.get("kind", "").asString();
	if (env.isMember("status"))
		line += " status=" + env.get("status", "").asString();
	if (env.isMember("dataset_id"))
		line += " dataset_id=" + env.get("dataset_id", "").asString();
	if (env.isMember("code"))
		line += " code=" + env.get("code", "").asString();

	Log::log() << line << std::endl;
}
