#include "viewfiller.h"

#include "dataviewbuffer.h"
#include "jaspclient/jaspclient.h"
#include "gui/preferencesmodel.h"
#include "qutils.h"
#include "log.h"

#include <QLocale>
#include <string>

ViewFiller::ViewFiller(const std::string & datasetId, DataViewBuffer * buffer, QObject * parent)
	: QObject(parent)
	, _datasetId(datasetId)
	, _buffer(buffer)
{
}

ViewFiller::~ViewFiller()
{
	stop();
}

void ViewFiller::start()
{
	if (_datasetId.empty() || !_buffer || !JaspClient::client())
		return;
	_stopState	= StopState::None;
	_filling	= true;
	requestNext();
	repairAfterRequest();	// a bail here (rare) must not leave a dangling filling state
}

void ViewFiller::stop()
{
	_filling = false;
	if (_inFlight && JaspClient::client())
		JaspClient::client()->abort(_inFlightWorkId);	// drops the handler slot — no callback after this
	_inFlight = false;
	_inFlightWorkId.clear();
}

void ViewFiller::repairAfterRequest()
{
	// INVARIANT (violated once against the real GUI): after any requestNext call, either a
	// request is in flight or the filler is in a wakeable stop state. A bail at requestNext's
	// entry guard (planning re-entrancy, a stray in-flight flag) leaves `_filling=true` with
	// nothing running — every later viewport change then sees "already filling" and never
	// wakes: the view stops loading. Repair it HERE, event-driven: the next viewport motion
	// is the retry.
	if (_filling && !_inFlight && !_planning)
	{
		Log::log() << "ViewFiller: repaired idle-filling state (requestNext bailed) — back to wakeable" << std::endl;
		_filling = false;
	}
}

void ViewFiller::setViewport(uint64_t firstRow, uint64_t lastRow)
{
	if (!_buffer)
		return;

	const uint64_t rowsTotal = _buffer->rowsTotal();
	firstRow	= std::min(firstRow, rowsTotal);
	lastRow		= std::min(std::max(lastRow, firstRow), rowsTotal);
	if (firstRow == _vpFirst && lastRow == _vpLast)
		return;		// viewportChangedDelayed also fires per chunk arrival (view rebuild) — ignore repeats

	_vpFirst	= firstRow;
	_vpLast		= lastRow;

	// Wake any non-Completed stop state: budget-stopped fills get a miss to evict + fetch for,
	// and a FAILED fill gets a free retry (a moved viewport is the natural retry trigger — one
	// request per move; a stationary failure stays quiet and keeps its error note).
	if (!_filling && _stopState != StopState::Completed)
	{
		_filling = true;
		requestNext();
		repairAfterRequest();	// a bail must not strand the filler in idle-filling (the s-21 bug)
	}
}

void ViewFiller::requestNext()
{
	if (_planning || !_filling || _inFlight || !_buffer || _datasetId.empty())
	{
		// Anomaly detector: the wake path sets _filling=true then calls this — bailing here
		// would leave the filler claiming to fill while idle (the s-21 stuck state; repaired
		// by repairAfterRequest at the call sites). A healthy loop never takes this path while
		// filling, so this line in a log is a real signal: name the guard that tripped.
		if (_filling)
			Log::log() << "ViewFiller: requestNext bailed at guard (planning=" << _planning
					   << " inFlight=" << _inFlight << " buffer=" << bool(_buffer)
					   << " id=" << _datasetId << ")" << std::endl;
		return;
	}

	// Re-entrancy guard: eviction emits chunksEvicted → GridModel dataChanged → view rebuild →
	// viewportChangedDelayed → setViewport → requestNext. The re-entrant call stores the new
	// viewport and returns here; this plan (computed from current residency, which nothing
	// changed) stays disjoint-safe, and the next cycle re-plans viewport-fresh.
	_planning = true;
	struct Guard { bool & _b; ~Guard() { _b = false; } } guard{ _planning };

	if (_buffer->complete())
	{
		_filling = false;
		if (_stopState != StopState::Completed)
		{
			_stopState = StopState::Completed;
			emit fillCompleted();
		}
		return;
	}

	const uint64_t rowsTotal	= _buffer->rowsTotal();
	const uint64_t protFirst	= _vpFirst > kViewportMargin ? _vpFirst - kViewportMargin : 0;
	const uint64_t protLast		= std::min(_vpLast + kViewportMargin, rowsTotal);

	// Class 1 — URGENT: the viewport ± margins must be resident (hard guarantee, format doc §2.5).
	uint64_t gap = _buffer->firstMissingRow(protFirst);
	if (gap < protLast && _buffer->budgetFull(kUrgentChunkBytes))
	{
		// Eviction only makes room (§2.5): farthest from the viewport first, the protected
		// window untouchable. Reclaim one background chunk of headroom as well — after a jump
		// the background fill re-anchors around the viewport instead of starving behind cold
		// data from the old location.
		_buffer->evictFarthest(size_t(kUrgentChunkBytes + kChunkBytes), protFirst, protLast);
		gap = _buffer->firstMissingRow(protFirst);		// residency changed under us — re-read
	}

	uint64_t	off		= 0,
				limit	= 0;
	quint64		maxBytes= kChunkBytes;

	if (gap < protLast)
	{
		off		= gap;
		limit	= std::min(_buffer->nextResidentStart(gap), protLast) - gap;
		maxBytes= kUrgentChunkBytes;
	}
	else
	{
		// Class 2 — BACKGROUND: fill outward from the viewport, DOWN first. The pre-request
		// budget check stands (§2.3) — eviction is the urgent class's privilege, so a full
		// buffer holds instead of rolling through the dataset on the wire forever.
		if (_buffer->budgetFull(kChunkBytes))
		{
			_filling = false;
			if (_stopState != StopState::Budget)
			{
				Log::log() << "ViewFiller: background stopped at budget (bytes=" << _buffer->bytes()
						   << " resident=" << _buffer->residentRows() << " of " << rowsTotal
						   << ") — viewport motion resumes urgent fetches" << std::endl;
				_stopState = StopState::Budget;
				emit budgetReached(_buffer->residentRows(), rowsTotal);
			}
			return;
		}

		gap = _buffer->firstMissingRow(protLast);
		if (gap < rowsTotal)
		{
			off		= gap;
			limit	= std::min(_buffer->nextResidentStart(gap), rowsTotal) - gap;
		}
		else
		{
			uint64_t upStart = 0, upEnd = 0;
			if (_buffer->lastMissingRunBelow(protFirst, upStart, upEnd))
			{
				off		= upStart;
				limit	= upEnd - upStart;
			}
			else
			{
				// Nothing urgent, nothing below, nothing above — everything resident. (Caught
				// by the complete() check in normal flow; this is the same conclusion.)
				_filling = false;
				if (_stopState != StopState::Completed)
				{
					_stopState = StopState::Completed;
					emit fillCompleted();
				}
				return;
			}
		}
	}

	if (limit == 0)
	{
		// Unreachable (the planners only produce non-empty runs) — but IF it ever happens, stop
		// in the wakeable state so a viewport move can revive the scheduler instead of zombieing.
		_filling = false;
		_stopState = StopState::Budget;
		return;
	}

	const std::string workId = "data-view-" + std::to_string(_nextWork++);

	Json::Value payload(Json::objectValue);
	payload["op"]			= "data_view";
	payload["source"]		= "";
	payload["cache_path"]	= "";		// orchestrator injects the dataset's current path at dispatch
	payload["format"]		= "";		// view is format-agnostic
	payload["ingest"]		= Json::Value(Json::objectValue);	// forwarded for lane statelessness; unused by view
	payload["row_offset"]	= Json::UInt64(off);
	payload["row_limit"]	= Json::UInt64(limit);
	payload["columns"]		= Json::nullValue;	// all columns, schema order (v1)
	payload["max_bytes"]	= Json::UInt64(maxBytes);
	payload["render"]		= renderSpec();

	Json::Value datasetIds(Json::arrayValue);
	datasetIds.append(_datasetId);

	Json::Value work(Json::objectValue);
	work["v"]			= 1;
	work["type"]		= "work";
	work["id"]			= workId;
	work["work_id"]		= workId;
	work["revision"]	= 0;
	work["dataset_ids"]	= datasetIds;
	work["kind"]		= "data";
	work["payload"]		= payload;

	_inFlight		= true;
	_inFlightWorkId	= workId;
	const uint64_t epoch = _buffer->epoch();

	JaspClient::client()->submit(work, [this, epoch, workId](const JaspClient::Result & result)
	{
		if (!_filling)
		{
			_inFlight = false;
			_inFlightWorkId.clear();
			return;									// stopped in the meantime (switch/close)
		}

		if (result.status == "running")
		{	// Park marker: the work sits parked in the orchestrator while its lane boots —
			// it is still in flight. Do NOT resubmit (that would park a second work for the
			// same offset); the same handler receives the real result when the lane serves it.
			_inFlight		= true;
			_inFlightWorkId	= workId;
			return;
		}

		_inFlight = false;
		_inFlightWorkId.clear();

		if (result.status != "complete")
		{
			// Transient failures retry a bounded number of times before surfacing (the lane can
			// hiccup — a lost file handle, a momentary dataset_not_ready); after giving up, viewport
			// motion still revives the scheduler (setViewport wakes Failed too).
			if (++_failStreak <= kMaxFailRetries)
			{
				Log::log() << "ViewFiller: data_view failed (" << result.status << "): " << result.message
						   << " — retrying (" << _failStreak << "/" << kMaxFailRetries << ")" << std::endl;
				requestNext();
				return;
			}
			_failStreak	= 0;
			_filling	= false;
			_stopState	= StopState::Failed;
			Log::log() << "ViewFiller: data_view failed (" << result.status << "): " << result.message << std::endl;
			emit fillFailed(tq(result.message));	// resident chunks kept; a viewport move retries
			return;
		}
		if (epoch != _buffer->epoch())
			return;									// stale fill identity — superseded by a drop/refill (§2.3)

		if (!_buffer->ingest(result.datasetRevision, epoch, result.rowOffset, result.rowCount, result.binary))
		{
			// Rejections split benign/genuine: a DUPLICATE delivery (everything it carried is
			// already resident — chunks are idempotent by revision + offset) just continues the
			// loop; only a genuinely unusable chunk (stale revision, out of range) is a failure.
			if (_buffer->isRangeResident(result.rowOffset, result.rowCount))
			{
				Log::log() << "ViewFiller: duplicate chunk " << result.rowOffset << "+" << result.rowCount
						   << " already resident — ignored" << std::endl;
				requestNext();
				return;
			}
			_failStreak	= 0;
			_filling	= false;
			_stopState	= StopState::Failed;
			Log::log() << "ViewFiller: chunk rejected by the buffer (stale or out of range)" << std::endl;
			emit fillFailed("View chunk rejected (stale or out of order).");
			return;
		}
		if (_failStreak > 0 || _stopState == StopState::Failed)
		{
			_failStreak	= 0;
			_stopState	= StopState::None;
			emit fillRecovered();		// clear the error note — data is flowing again
		}
		requestNext();								// keep the loop running — urgent first, then background
		repairAfterRequest();						// a bail (re-entrancy) must not strand the loop
	});
}

Json::Value ViewFiller::renderSpec() const
{
	// The lane is the only float→string converter (format doc §1.2): the frontend's QLocale
	// is the authority and sends its separators with every request. Legacy parity: 'g' at 10
	// significant digits (QLocale::toString(dbl, 'g', 10)), grouping per the
	// useThousandSeparators preference.
	const PreferencesModel * prefs = PreferencesModel::prefs();
	const QLocale & locale = prefs ? prefs->localeQt() : QLocale::system();

	Json::Value render(Json::objectValue);
	render["decimal"]	= fq(locale.decimalPoint());
	render["thousands"]	= (prefs && prefs->useThousandSeparators()) ? fq(locale.groupSeparator()) : "";
	render["precision"]	= 10;
	return render;
}
