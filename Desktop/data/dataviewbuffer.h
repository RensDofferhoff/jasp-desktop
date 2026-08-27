#ifndef DATAVIEWBUFFER_H
#define DATAVIEWBUFFER_H

#include <QObject>
#include <QByteArray>
#include <QString>

#include <cstdint>
#include <deque>
#include <list>
#include <unordered_map>
#include <vector>

/// NEO view buffer — the frontend's resident copy of the active dataset's cells
/// (refactor_design/data-view-format.md §2 is the NORMATIVE layout).
///
/// Stores the lane's escaped-TSV chunks VERBATIM: no per-cell ownership anywhere — cells are
/// byte ranges into the chunk arenas, materialized to QString only for visible, requested
/// cells. Shape of the data:
///
///  - chunks — exactly what one `data_view` response delivered, kept sorted + disjoint by
///    rowOffset. Buffered fill appends at the frontier; sliding mode (format doc §2.5) inserts
///    urgent viewport-miss chunks out of order — the sorted/disjoint invariant is checked on
///    every ingest, never assumed from arrival order;
///  - row anchors — one byte offset every `kAnchorStride` rows (u32 — a chunk is < 4 GB), so
///    locating a row costs a scan of ≤ stride row-terminators, and only on a split-cache miss;
///  - split-row LRU — the grid's access is column-major over the VISIBLE rows (~6 role
///    queries per cell); a row is split into cell offsets once, then served O(1) per cell.
///    Evicting a chunk drops its split rows (they point into its arena).
///
/// One fill identity — `(revision, epoch, render spec)` — all resident chunks share it; a
/// refill (new revision, locale change) is a `reset()` + refill, never a mix. Main-thread only.
struct ViewChunk
{
	uint64_t				rowOffset = 0;		///< dataset row of this chunk's first row
	uint64_t				rowCount  = 0;
	QByteArray				tsv;				///< the binary part as received (whole LF-terminated rows)
	uint32_t				anchorStride = 32;	///< K: one anchor per K rows
	std::vector<uint32_t>	anchorByte;			///< anchor i -> byte offset of row (i*K) within tsv
};

class DataViewBuffer : public QObject
{
	Q_OBJECT

public:
	static constexpr size_t		kDefaultBudget	= 200000000;	///< ~200 MB soft budget (frontend policy, not a wire limit)
	static constexpr uint32_t	kAnchorStride	= 32;			///< one row anchor per 32 rows (~0.125 B/row)
	static constexpr size_t		kSplitCacheRows	= 128;			///< ≥ any plausible viewport height

	explicit DataViewBuffer(QObject * parent = nullptr);

	/// Begin a new fill identity: drop everything resident, adopt (rowsTotal, revision, epoch).
	/// The render spec is part of the identity but baked into the bytes — the buffer does not
	/// carry it (format doc §2.3).
	void		reset(uint64_t rowsTotal, uint64_t revision, uint64_t epoch, size_t budget = kDefaultBudget);

	/// Ingest one chunk (sliding mode, format doc §2.5): inserted SORTED by rowOffset — it does
	/// not have to continue the frontier. Returns false when the chunk does not belong to the
	/// current fill identity (stale revision/epoch), falls outside the dataset, or OVERLAPS a
	/// resident chunk (requests are planned from residency on a serial lane, so an overlap means
	/// a pathological reorder — chunks are idempotent by offset; drop, never merge). Emits
	/// chunkIngested on success.
	bool		ingest(uint64_t revision, uint64_t epoch, uint64_t rowOffset, uint64_t rowCount, const QByteArray & tsv);

	uint64_t	rowsTotal()		const	{ return _rowsTotal;	}
	uint64_t	revision()		const	{ return _revision;		}
	uint64_t	epoch()			const	{ return _epoch;			}
	uint64_t	frontier()		const	{ return _frontier;		}	///< highest ingested row + 1 (bookkeeping; sliding planners query residency instead)
	size_t		bytes()			const	{ return _bytes;			}	///< Σ tsv.size()
	size_t		budget()			const	{ return _budget;		}
	bool		complete()		const;									///< every dataset row resident (disjoint chunks ⇒ Σ rowCount == rowsTotal is a full cover)
	bool		isEmpty()		const	{ return _chunks.empty();	}
	uint64_t	residentRows()	const;									///< rows currently resident

	/// Sliding-mode planning queries (format doc §2.5) — the fill scheduler's only view of
	/// residency. All O(chunks); the chunk count stays small (budget / chunk size + a few
	/// urgent slivers), so no index structure is warranted.
	bool		isResident(uint64_t row)					const;	///< chunkFor(row) != nullptr
	bool		isRangeResident(uint64_t rowOffset, uint64_t rowCount)	const;	///< the whole [off, off+count) resident (duplicate-delivery detector)
	uint64_t	firstMissingRow(uint64_t from)			const;	///< first row of [from, …) not resident
	uint64_t	nextResidentStart(uint64_t from)			const;	///< start of the first chunk at/after `from` (rowsTotal when none)
	/// The topmost missing run strictly below `to` (the up-fill planner's target). Returns
	/// false when [0, to) is fully resident.
	bool		lastMissingRunBelow(uint64_t to, uint64_t & start, uint64_t & end)	const;

	/// Sliding-mode eviction (format doc §2.5 — "eviction only makes room"): drop the chunks
	/// FARTHEST from the protected window [keepFirst, keepLast) first until `incomingBytes`
	/// fits the budget. Chunks intersecting the window are never dropped — the viewport ±
	/// margins stay resident, hard guarantee. Split-row cache entries go with their chunk.
	/// Returns the bytes freed. The budget stays soft: when nothing droppable remains the call
	/// returns with less room than asked.
	size_t		evictFarthest(size_t incomingBytes, uint64_t keepFirst, uint64_t keepLast);

	/// Pre-request budget check (format doc §2.3): the budget is SOFT — the last accepted
	/// chunk may overshoot it, so the check happens before requesting.
	bool		budgetFull(size_t chunkBytes) const	{ return _bytes + chunkBytes > _budget;	}

	/// Cell access (format doc §2.2): the display string of (row, col); `isNull` = whole-cell
	/// `\N`. Returns false when the row is not resident or col out of range. Not const — the
	/// split-row LRU bookkeeping moves its entries.
	bool		cellAt(uint64_t row, int col, QString & text, bool & isNull);

signals:
	void		chunkIngested(quint64 firstRow, quint64 rowCount);
	void		chunksEvicted(quint64 firstRow, quint64 rowCount);	///< a chunk left the buffer — its rows render as placeholders again
	void		bufferReset();

private:
	struct SplitRow						// one entry per hot row (LRU)
	{
		uint64_t				globalRow = UINT64_MAX;
		uint32_t				rowStart = 0;		///< absolute byte offset of the row within its chunk
		std::vector<uint32_t>	cellStart;			///< ncols+1 ABSOLUTE byte offsets; last = end of the last cell
	};

	const ViewChunk *	chunkFor(uint64_t row) const;
	SplitRow &			splitRow(const ViewChunk & chunk, uint64_t row);
	void			dropSplitRows(uint64_t firstRow, uint64_t rowCount);
	static QString		unescapeCell(const char * begin, const char * end, bool & isNull);

	std::deque<ViewChunk>	_chunks;		///< sorted by rowOffset, disjoint (checked on ingest)
	uint64_t				_rowsTotal = 0;
	uint64_t				_revision  = 0;
	uint64_t				_epoch     = 0;
	uint64_t				_frontier  = 0;		///< max(rowOffset + rowCount) ever ingested in this identity
	size_t					_bytes     = 0;
	size_t					_budget    = kDefaultBudget;

	std::list<SplitRow>		_splitLru;		///< front = most recently used
	std::unordered_map<uint64_t, std::list<SplitRow>::iterator>	_splitIndex;	///< globalRow -> LRU entry
};

#endif // DATAVIEWBUFFER_H
