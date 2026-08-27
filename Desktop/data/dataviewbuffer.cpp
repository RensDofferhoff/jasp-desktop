#include "dataviewbuffer.h"

#include <algorithm>
#include <cstring>

DataViewBuffer::DataViewBuffer(QObject * parent)
	: QObject(parent)
{
}

void DataViewBuffer::reset(uint64_t rowsTotal, uint64_t revision, uint64_t epoch, size_t budget)
{
	_chunks.clear();
	_splitLru.clear();
	_splitIndex.clear();
	_rowsTotal	= rowsTotal;
	_revision	= revision;
	_epoch		= epoch;
	_budget		= budget;
	_bytes		= 0;
	_frontier	= 0;

	emit bufferReset();
}

bool DataViewBuffer::complete() const
{
	// Chunks are disjoint and contained in [0, rowsTotal) (both checked on ingest), so a total
	// resident-row count equal to rowsTotal can only mean a full cover.
	return residentRows() == _rowsTotal;
}

bool DataViewBuffer::ingest(uint64_t revision, uint64_t epoch, uint64_t rowOffset, uint64_t rowCount, const QByteArray & tsv)
{
	// One fill identity — (revision, epoch, render spec). Stale responses are dropped, never
	// mixed (format doc §2.3).
	if (revision != _revision || epoch != _epoch || rowCount == 0)
		return false;
	if (rowOffset >= _rowsTotal || rowOffset + rowCount > _rowsTotal)
		return false;

	// Sorted, DISJOINT insertion (sliding mode, format doc §2.5): an urgent viewport-miss chunk
	// may land anywhere. The serial lane + residency-planned requests make overlaps impossible
	// in practice — seeing one means a pathological reorder, and the idempotent answer is drop.
	const auto at = std::lower_bound(_chunks.begin(), _chunks.end(), rowOffset,
							  [](const ViewChunk & c, uint64_t off) { return c.rowOffset < off; });
	if (at != _chunks.begin() && std::prev(at)->rowOffset + std::prev(at)->rowCount > rowOffset)
		return false;
	if (at != _chunks.end() && rowOffset + rowCount > at->rowOffset)
		return false;

	ViewChunk chunk;
	chunk.rowOffset		= rowOffset;
	chunk.rowCount		= rowCount;
	chunk.tsv			= tsv;
	chunk.anchorStride	= kAnchorStride;
	// Row anchors: one byte offset every K rows. Grammar guarantees real LF bytes only ever
	// occur as row terminators (embedded LFs ship escaped), so the anchor scan is a raw scan.
	chunk.anchorByte.reserve(size_t(rowCount / kAnchorStride) + 1);
	chunk.anchorByte.push_back(0);
	uint64_t rows = 0;
	const char * d = tsv.constData();
	for (int i = 0; i < tsv.size(); ++i)
		if (d[i] == '\n' && ++rows < rowCount && rows % kAnchorStride == 0)
			chunk.anchorByte.push_back(uint32_t(i + 1));	// row `rows` starts right after this LF

	_bytes		+= size_t(tsv.size());
	_frontier	= std::max(_frontier, rowOffset + rowCount);
	_chunks.insert(at, std::move(chunk));

	emit chunkIngested(quint64(rowOffset), quint64(rowCount));
	return true;
}

uint64_t DataViewBuffer::residentRows() const
{
	uint64_t n = 0;
	for (const ViewChunk & c : _chunks)
		n += c.rowCount;
	return n;
}

const ViewChunk * DataViewBuffer::chunkFor(uint64_t row) const
{
	// Binary search: chunks are sorted by rowOffset and disjoint.
	size_t lo = 0, hi = _chunks.size();
	while (lo < hi)
	{
		const size_t mid = lo + (hi - lo) / 2;
		if (_chunks[mid].rowOffset <= row)	lo = mid + 1;
		else									hi = mid;
	}
	if (lo == 0)
		return nullptr;
	const ViewChunk & c = _chunks[lo - 1];
	return row < c.rowOffset + c.rowCount ? &c : nullptr;
}

bool DataViewBuffer::isResident(uint64_t row) const
{
	return chunkFor(row) != nullptr;
}

bool DataViewBuffer::isRangeResident(uint64_t rowOffset, uint64_t rowCount) const
{
	// A duplicate delivery is harmless exactly when everything it carried is already here —
	// then rejecting the ingest (overlap) is a no-op, not an error (chunks are idempotent by
	// revision + offset, format doc §2.3).
	return rowCount > 0 && firstMissingRow(rowOffset) >= rowOffset + rowCount;
}

uint64_t DataViewBuffer::firstMissingRow(uint64_t from) const
{
	uint64_t at = from;
	for (const ViewChunk & c : _chunks)
	{
		if (c.rowOffset + c.rowCount <= at)	continue;		// entirely below the cursor
		if (c.rowOffset > at)				break;			// disjoint + sorted: a gap starts at `at`
		at = c.rowOffset + c.rowCount;						// walk through the chunk
	}
	return at;
}

uint64_t DataViewBuffer::nextResidentStart(uint64_t from) const
{
	for (const ViewChunk & c : _chunks)
		if (c.rowOffset >= from)
			return c.rowOffset;
	return _rowsTotal;
}

bool DataViewBuffer::lastMissingRunBelow(uint64_t to, uint64_t & start, uint64_t & end) const
{
	// Walk the (sorted, disjoint) chunks top-down with a cursor at `to`, skipping chunks that
	// sit above it; the first chunk that ends strictly below the cursor reveals the gap between
	// its end and the cursor — the topmost missing run below `to`.
	uint64_t hi = to;
	for (size_t i = _chunks.size(); i-- > 0; )
	{
		const ViewChunk & c = _chunks[i];
		if (c.rowOffset >= hi)
			continue;									// entirely above the cursor
		if (c.rowOffset + c.rowCount < hi)
		{
			start	= c.rowOffset + c.rowCount;
			end		= hi;
			return true;
		}
		hi = c.rowOffset;							// contiguous with (or covering) the cursor — keep walking
		if (hi == 0)
			break;
	}
	if (hi > 0)
	{
		start	= 0;
		end		= hi;
		return true;									// nothing resident below the cursor at all
	}
	return false;
}

size_t DataViewBuffer::evictFarthest(size_t incomingBytes, uint64_t keepFirst, uint64_t keepLast)
{
	size_t freed = 0;
	while (_bytes + incomingBytes > _budget)
	{
		// Farthest 1D distance from the protected window (format doc §2.5) — chunks intersecting
		// it are not candidates, so the viewport ± margins can never be evicted out from under
		// the grid.
		size_t		far		= 0;
		uint64_t	farDist	= 0;
		bool		found	= false;
		for (size_t i = 0; i < _chunks.size(); ++i)
		{
			const ViewChunk & c = _chunks[i];
			uint64_t dist;
			if (c.rowOffset + c.rowCount <= keepFirst)	dist = keepFirst - (c.rowOffset + c.rowCount);
			else if (c.rowOffset >= keepLast)			dist = c.rowOffset - keepLast;
			else												continue;	// protected
			if (!found || dist > farDist)
			{
				found	= true;
				far		= i;
				farDist	= dist;
			}
		}
		if (!found)
			break;	// everything left is protected — the budget stays soft (§2.3)

		const uint64_t	firstRow	= _chunks[far].rowOffset,
						rows		= _chunks[far].rowCount;
		freed	+= size_t(_chunks[far].tsv.size());
		_bytes	-= size_t(_chunks[far].tsv.size());
		dropSplitRows(firstRow, rows);
		_chunks.erase(_chunks.begin() + long(far));

		emit chunksEvicted(quint64(firstRow), quint64(rows));
	}
	return freed;
}

void DataViewBuffer::dropSplitRows(uint64_t firstRow, uint64_t rowCount)
{
	// The split cache is tiny (≤ kSplitCacheRows) — a range scan over the LRU beats keeping a
	// per-chunk index. Entries below the range would dangle into a dead arena otherwise.
	const uint64_t last = firstRow + rowCount;
	for (auto it = _splitLru.begin(); it != _splitLru.end(); )
	{
		if (it->globalRow >= firstRow && it->globalRow < last)
		{
			_splitIndex.erase(it->globalRow);
			it = _splitLru.erase(it);
		}
		else
			++it;
	}
}

DataViewBuffer::SplitRow & DataViewBuffer::splitRow(const ViewChunk & chunk, uint64_t row)
{
	auto found = _splitIndex.find(row);
	if (found != _splitIndex.end())
	{
		_splitLru.splice(_splitLru.begin(), _splitLru, found->second);	// MRU to the front
		return *found->second;
	}

	// First touch of this row: locate it from its anchor (forward scan of ≤ stride
	// row-terminators — raw LF scan, escape-free by the grammar), then one raw TAB scan for
	// the cell offsets (real TABs never survive escaping — separators are byte-unambiguous,
	// format doc §1.2).
	const uint64_t idx		= row - chunk.rowOffset;
	const uint32_t anchor	= chunk.anchorByte[idx / chunk.anchorStride];
	const char * d			= chunk.tsv.constData();
	const int    n			= chunk.tsv.size();

	int pos = int(anchor);
	for (uint64_t skip = idx % chunk.anchorStride; skip > 0; --skip)
	{
		while (pos < n && d[pos] != '\n')
			++pos;
		++pos;	// past the LF
	}

	_splitLru.emplace_front();
	SplitRow & sr = _splitLru.front();
	sr.globalRow	= row;
	sr.rowStart		= uint32_t(pos);
	sr.cellStart.push_back(uint32_t(pos));
	while (pos < n && d[pos] != '\n')
	{
		if (d[pos] == '\t')
			sr.cellStart.push_back(uint32_t(pos + 1));	// next cell starts after the TAB
		++pos;
	}
	sr.cellStart.push_back(uint32_t(pos));	// end of the last cell (at the LF / buffer end)

	while (_splitIndex.size() >= kSplitCacheRows)
	{
		_splitIndex.erase(_splitLru.back().globalRow);
		_splitLru.pop_back();
	}
	_splitIndex[row] = _splitLru.begin();
	return sr;
}

bool DataViewBuffer::cellAt(uint64_t row, int col, QString & text, bool & isNull)
{
	text.clear();
	isNull = false;

	const ViewChunk * chunk = chunkFor(row);
	if (!chunk || col < 0)
		return false;

	SplitRow & split = splitRow(*chunk, row);
	if (size_t(col) + 1 >= split.cellStart.size())
		return false;

	const char * base	= chunk->tsv.constData();
	const char * begin	= base + split.cellStart[col];
	const char * end	= base + split.cellStart[col + 1];
	// cellStart entries are cell STARTS — for every cell but the last, the boundary byte is
	// the TAB separator: it belongs to the grammar, not the cell. (The last cell's boundary
	// is the row's LF / buffer end, already excluded.) Getting this wrong smuggles a trailing
	// TAB into every cell — invisible on screen, but it breaks the exact-match `\N` null check.
	if (size_t(col) + 2 < split.cellStart.size())
		--end;
	text = unescapeCell(begin, end, isNull);
	return true;
}

QString DataViewBuffer::unescapeCell(const char * begin, const char * end, bool & isNull)
{
	isNull = false;
	const size_t len = size_t(end - begin);

	// Null marker: the WHOLE cell `\N`. A literal `\N` cell ships escaped as `\\N` (3 bytes)
	// and takes the normal unescape path — unambiguous by construction (format doc §1.2).
	if (len == 2 && begin[0] == '\\' && begin[1] == 'N')
	{
		isNull = true;
		return QString();
	}

	// Fast path: no backslash → the bytes are the cell verbatim (most cells).
	if (!std::memchr(begin, '\\', len))
		return QString::fromUtf8(begin, int(len));

	std::string buf;
	buf.reserve(len);
	for (size_t i = 0; i < len; ++i)
	{
		const char c = begin[i];
		if (c == '\\' && i + 1 < len)
		{
			const char e = begin[++i];
			switch (e)
			{
			case 't':	buf.push_back('\t');	break;
			case 'n':	buf.push_back('\n');	break;
			case 'r':	buf.push_back('\r');	break;
			case '\\':	buf.push_back('\\');	break;
			default:	// cannot occur in the normative grammar — keep both bytes, visibly
				buf.push_back('\\');
				buf.push_back(e);
				break;
			}
		}
		else
			buf.push_back(c);
	}
	return QString::fromUtf8(buf.data(), int(buf.size()));
}
