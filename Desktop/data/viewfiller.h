#ifndef VIEWFILLER_H
#define VIEWFILLER_H

#include <QObject>
#include <QString>
#include <cstdint>
#include <string>

#include <json/value.h>

class DataViewBuffer;
class DataViewBuffer;

/// The ACTIVE dataset's view-fill scheduler (refactor_design/data-view-design.md §7.4,
/// format doc §2.5) — the two-class prefetch loop.
///
/// Owned by the DatasetRegistry together with its buffer — singular by construction
/// (design §7.6): created when a dataset becomes active, stopped + dropped on
/// switch/close/reset. One `data_view` in flight at a time (the lane is serial, so
/// pipelining gains nothing); "priority" just decides what the NEXT request is:
///
///  1. URGENT — viewport miss: rows in [viewport ± kViewportMargin] that are not resident
///     get fetched first, at a smaller chunk size for lower latency. These requests may
///     EVICT to make room (farthest-from-viewport first, the window itself untouchable —
///     "viewport ± margins is always resident" is a hard guarantee). Eviction also reclaims
///     one background chunk of headroom so the background fill re-anchors after a jump.
///  2. BACKGROUND — keep the budget full: fill outward from the viewport, DOWN first
///     (scrolling down dominates), then up. Stops at the budget — eviction is reserved for
///     the urgent class, so a full buffer stays put instead of churning wire traffic.
///
/// The viewport arrives via setViewport() (the grid wires its viewportChangedDelayed);
/// default is (0, 0) — the top of the dataset, exactly the pre-sliding sequential fill.
///
/// Stop states (reached only through requestNext; transitions emit once):
///  - every dataset row resident          → fillCompleted
///  - background at budget, viewport ok   → budgetReached (wakes on the next viewport move)
///  - lane error / rejected chunk         → fillFailed   (resident chunks kept; no auto-retry)
///
/// Fill identity guard (format doc §2.3): the epoch captured at submission is checked on
/// arrival — a response landing after a drop/refill is discarded, never ingested.
class ViewFiller : public QObject
{
	Q_OBJECT

public:
	static constexpr uint64_t	kChunkBytes			= 20000000;	///< background chunk ~20 MB (format doc §1.1)
	static constexpr uint64_t	kUrgentChunkBytes		= 2000000;	///< viewport-miss chunk ~2 MB — lower latency
	static constexpr uint64_t	kViewportMargin		= 1024;		///< rows kept resident around the viewport (test-tunable)
	static constexpr int		kMaxFailRetries		= 2;			///< transient-failure auto-retries before giving up (viewport motion still revives)

	explicit ViewFiller(const std::string & datasetId, DataViewBuffer * buffer, QObject * parent = nullptr);
	~ViewFiller() override;

	void	start();			///< begin the fill (viewport at the top — the urgent class covers [0, margin))
	void	stop();				///< detach from the wire (aborts an in-flight chunk); the buffer stays

	/// The grid's current viewport row range [firstRow, lastRow) (max-exclusive, as the view
	/// computes it). Clamped, stored, and kicks the scheduler: any non-Completed stop state
	/// wakes on viewport motion — budget-stopped (a miss can now evict + fetch) and failed
	/// alike (a moved viewport is a free, natural retry; a stationary failed fill stays quiet).
	/// Completed has nothing left to do.
	void	setViewport(uint64_t firstRow, uint64_t lastRow);

	bool	filling() const		{ return _filling; }

signals:
	void	fillCompleted();							///< whole dataset resident
	void	budgetReached(quint64 rowsResident, quint64 rowsTotal);	///< background fill at budget; viewport served
	void	fillFailed(QString message);			///< lane error / rejected chunk; retryable
	void	fillRecovered();							///< a chunk landed after a failure — clear the error note

private:
	void		requestNext();
	void		repairAfterRequest();	///< enforce the post-requestNext invariant: in-flight or wakeable, never idle-filling
	Json::Value	renderSpec() const;

	enum class StopState { None, Completed, Budget, Failed };

	std::string				_datasetId;	///< the orchestrator dataset id — every data-view request names it
	DataViewBuffer	*	_buffer;
	///< view work ids are PROCESS-GLOBAL (see the counter in the .cpp) — never reset here,
	///< or a restarted view reuses in-flight ids and its late results burn the new slots
	bool				_filling		= false;
	bool				_inFlight		= false;
	bool				_planning		= false;	///< re-entrancy guard: eviction emits chunksEvicted → view rebuild → viewportChangedDelayed → setViewport → requestNext
	StopState			_stopState		= StopState::None;
	int				_failStreak		= 0;		///< consecutive failures without a success (auto-retry budget)
	uint64_t			_vpFirst			= 0;	///< viewport first row (inclusive)
	uint64_t			_vpLast				= 0;	///< viewport last row (EXCLUSIVE)
	std::string			_inFlightWorkId;
};

#endif // VIEWFILLER_H
