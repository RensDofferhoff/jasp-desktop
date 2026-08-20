#ifndef JASPCLIENT_H
#define JASPCLIENT_H

#include <atomic>
#include <cstdint>
#include <functional>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <utility>

#include <QObject>
#include <QByteArray>

#include <nng/nng.h>
#include <json/json.h>

#include "catalogmodule.h"

/// NEO alpha client for the orchestrator (refactor_design/neo-jasp.md §5.1).
///
/// The anti-slop contract this class exists to enforce:
///  - Any entity submits work / data edits / commands by calling a plain method. No polling,
///    no mutating a status and hoping a scheduler notices (this is what killed `shouldRun`).
///  - Results are routed by a correlation map (`work_id -> {revision, handler}`), i.e. plain C++
///    lookup — NOT a broadcast signal that every consumer connects to and filters. `work_id` is the
///    stable id of a logical work unit (for analyses, the analysis instance id, §19.1); re-submitting
///    with a higher revision evicts the in-flight slot and stale results are dropped by revision (§23).
///  - Connection is a handshake (§17.2/§19.5): a transient REQ `hello` on the control endpoint
///    yields a `welcome` carrying a dedicated PAIR data-channel URL + the orchestrator-assigned
///    `session_id`; all work/result then flows on that channel, not the control endpoint.
///  - Module discovery is pure-receiver: the orchestrator pushes the catalog as the FIRST frame
///    on the data channel (buffered at channel setup) and again on every change. The client
///    caches it (`catalog()`) and emits `modulesUpdated`; it never queries and never triggers.
///    Reply-vs-push is collapsed inside this class — consumers cannot tell them apart and per
///    neo-jasp.md §19.2 should not: both mean "replace your menu".
///  - A single worker thread owns the handshake (retrying until the orchestrator is up), the
///    data-channel dial, reconnect-on-drop (a re-handshake), and the blocking `recv`. Each incoming
///    frame is handed to the main (GUI) thread through a *single* queued invocation, and the handler
///    runs there (Qt objects are main-thread-only). No QTimer polling, no signal/slot routing web.
///
/// The rest of the frontend talks to this facade; it never sees a socket, a thread, or a wire
/// format.
class JaspClient : public QObject
{
	Q_OBJECT
public:
	/// How verbosely JaspClient logs wire IO (messages sent and received).
	/// Named `Off` rather than `None` because X11 defines `None` as a macro on Linux.
	enum class Verbosity
	{
		Off,	///< log nothing
		Stub,	///< one-line summary per message (type / work_id / kind / status)
		Full	///< the full JSON of every message
	};

	/// A `result` decoded by kind (§19.2 "Result payloads by kind"). JaspClient switches on
	/// `kind` and fills the matching field group; a handler reads only its own kind's fields.
	/// Replaces the old positional (results, status, progress, resultsDir) blob — `progress`
	/// returns when it is real.
	struct Result
	{
		std::string kind;		///< "analysis_r_classic_jaspbase" | "data" ("rcode" reserved)
		std::string status;		///< §22 status string

		// kind:"analysis_r_classic_jaspbase" — the jaspResults tree + its asset bootstrap.
		Json::Value results;	///< the opaque jaspResults tree; on failure the error tree {error, errorMessage, title}
		std::string resultsDir;	///< orchestrator's per-revision artifact dir; wire-only, never persisted

		// kind:"data" — the terminal dataset-open outcome (replaces `dataset_ready`).
		std::string datasetId;	///< the minted identity to reference in later work (dataset_ids)
		uint64_t    rows = 0;	///< row count
		Json::Value schema;		///< column view [{name, display_name, type, levels?, all_integer?}]

		std::string message;	///< human-readable detail on failure (payload error_message / result message)
	};

	/// Invoked on the main thread for every `result` of a submitted work unit, i.e.
	/// `submit(work) -> stream<result>`.
	using ResultHandler = std::function<void(const Result & result)>;

	static JaspClient * client();	///< singleton (created with the application)

	explicit JaspClient(QObject * parent = nullptr);
	~JaspClient() override;

	/// Start the connection worker: handshake (REQ `hello` → `welcome`) on the control endpoint,
	/// then dial the dedicated PAIR data channel and receive on it. Non-blocking: if the orchestrator
	/// is not up yet, the worker retries the handshake in the background, and re-handshakes
	/// automatically if the data channel later drops.
	void connectToOrchestrator(const std::string & url);

	/// Submit a fully-formed `work` envelope; `handler` receives each result until a terminal
	/// status. The envelope's `work_id` keys the correlation slot (§19.1):
	///  - Same `work_id` == same logical work unit. Re-submitting it with a higher `revision`
	///    evicts the in-flight handler and supersedes stale results — this is how "options changed"
	///    is expressed. (For analyses, `work_id` is the stable analysis instance id.)
	///  - If `work_id` is omitted, the client mints a unique one (generic one-shot work).
	///
	/// CONTRACT: if you supply your own `work_id`, YOU guarantee it is unique per logical work unit
	/// within the session — two distinct work units must never share a `work_id`. The client cannot
	/// enforce this (a re-submission and a collision are indistinguishable to it), so uniqueness is
	/// the caller's responsibility. Returns the `work_id`.
	std::string submit(const Json::Value & work, ResultHandler handler);

	/// Cancel a work unit and drop its handler.
	void abort(const std::string & workId);

	bool connected() const { return _connected; }

	/// Last module catalog received. Empty until the first `modules` message; held across a
	/// channel drop (the server re-pushes on re-handshake, so reconnects refresh it). GUI-thread
	/// state: the module machinery reads it once when ready, then tracks `modulesUpdated`.
	const ModuleCatalog & catalog() const { return _catalog; }

	/// Set how verbosely wire IO is logged. Also configurable via the JASP_CLIENT_LOG env var
	/// ("off" / "stub" / "full"), read at construction.
	void		setVerbosity(Verbosity v)	{ _verbosity = v; }
	Verbosity	verbosity() const			{ return _verbosity; }

signals:
	/// The current module catalog — emitted on every `modules` message: the connect-time push
	/// (first frame on the data channel), change pushes, and any list_modules reply. GUI thread.
	/// Replace your menu with it.
	void modulesUpdated(const ModuleCatalog & catalog);

private:
	// Connection lifecycle — all run on the single worker thread (`_recvThread`).
	void connectionLoop();						///< handshake (retry) -> dial channel -> recvLoop -> re-handshake on drop
	bool handshake();							///< REQ hello -> welcome; sets _channelUrl + _sessionId
	bool openDataChannel();						///< PAIR open + buffers + dial _channelUrl
	void closeDataChannel();					///< close _socket (under _socketMutex)
	void recvLoop();							///< bounded recv on _socket -> queue to main; returns on drop/shutdown
	static void pipeEventCb(nng_pipe pipe, nng_pipe_ev ev, void * arg);	///< flags channel drop (NNG thread)
	// Main (GUI) thread.
	void handleMessage(const QByteArray & body);	///< parse + dispatch
	static ModuleCatalog parseCatalog(const Json::Value & modulesJson);	///< `modules` array → typed entries (tolerant)
	void sendFrame(const Json::Value & envelope);	///< frame + send (under _socketMutex)
	void logIo(const char * dir, const Json::Value & env) const;	///< log one message per _verbosity
	// Framing (§18.1).
	static QByteArray frameEnvelope(const Json::Value & env);
	/// Split a frame into its JSON envelope and trailing binary payload (§18.1). View
	/// results carry their escaped-TSV cells in the tail — consumers get raw bytes that
	/// never went through the JSON parser. The tail is empty on JSON-only frames.
	static std::pair<Json::Value, QByteArray> splitFrame(const QByteArray & body);
	static Json::Value deframeEnvelope(const QByteArray & body);

	static JaspClient * _singleton;

	std::string			_url;					///< control endpoint (REQ/REP handshake)
	std::string			_channelUrl;			///< data-channel URL from welcome
	std::string			_sessionId;				///< orchestrator-assigned session (envelope of welcome)

	std::mutex			_socketMutex;			///< guards _socket/_socketOpen across main (send) and worker (open/close)
	nng_socket			_socket;				///< PAIR data channel
	bool				_socketOpen = false;
	std::thread			_recvThread;			///< runs connectionLoop
	std::atomic<bool>	_running{ false };
	std::atomic<bool>	_connected{ false };
	std::atomic<bool>	_channelDropped{ false };	///< set by pipeEventCb when the PAIR pipe is removed
	Verbosity			_verbosity = Verbosity::Off;	///< wire-IO log level (main thread only)

	int							_nextWork = 0;
	int							_nextHello = 0;

	/// Correlation slot for one logical work unit. Keyed by `work_id` — which for analyses IS the
	/// stable analysis instance id (§19.1). Re-submitting the same `work_id` with a higher revision
	/// REPLACES the slot (eviction by assignment); a result whose `revision` is below the slot's is
	/// stale and dropped. No per-analysis plumbing and no id-mapping — the stored revision doubles
	/// as the per-`work_id` high-water mark (§23).
	struct Slot
	{
		uint64_t		revision;
		std::string		kind;	///< the submitted work's kind — shapes synthetic failures surfaced via `error` messages
		ResultHandler	handler;
	};
	std::map<std::string, Slot>	_slots;	///< work_id -> {revision, handler} (main thread only)

	ModuleCatalog	_catalog;	///< last module catalog received (main thread only)
};

#endif // JASPCLIENT_H
