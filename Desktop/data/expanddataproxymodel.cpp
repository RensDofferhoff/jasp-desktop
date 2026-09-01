#include "expanddataproxymodel.h"
#include "datasettablemodel.h"
#include "dataenums.h"
#include "qutils.h"
#include "workspace.h"
#include "gridmodel.h"
#include "jaspclient/dataedit.h"
#include "log.h"
#include <algorithm>
#include <climits>

ExpandDataProxyModel::ExpandDataProxyModel(QObject *parent)
	: QIdentityProxyModel{parent}
{
	connectUndoStack();

	if (Workspace::singleton())
		connect(Workspace::singleton(), &Workspace::shownDataSetChanged, this, &ExpandDataProxyModel::onCurrentUndoStackChanged);
}

void ExpandDataProxyModel::connectUndoStack()
{
	if (_undoChangedCon)
		disconnect(_undoChangedCon);

	if (auto* stack = UndoStack::singleton())
		_undoChangedCon = connect(stack, &QUndoStack::indexChanged, this, &ExpandDataProxyModel::undoChanged);
}

void ExpandDataProxyModel::onCurrentUndoStackChanged()
{
	connectUndoStack();
	emit undoChanged();
}

int ExpandDataProxyModel::rowCount(const QModelIndex &) const
{
	if (!sourceModel())
		return 0;
	return sourceModel()->rowCount() + (_expandDataSet ? EXTRA_ROWS : 0);
}

int ExpandDataProxyModel::columnCount(const QModelIndex &) const
{
	if (!sourceModel())
		return 0;
	return sourceModel()->columnCount() + (_expandDataSet ? EXTRA_COLS : 0);
}

QVariant ExpandDataProxyModel::data(const QModelIndex &indexP, int role) const
{
	if (!sourceModel() || role == -1) // Role not defined
		return QVariant();

	int row		= indexP.row(),
		column	= indexP.column();

	// Real cell: forward to the (filtered) source model so the column/row filters are respected.
	if (column < sourceModel()->columnCount() && row < sourceModel()->rowCount())
		return sourceModel()->data(sourceModel()->index(row, column), role);

	// Virtual cell (past the filtered region, only present in expand mode): synthesize.
	switch(role)
	{
	case int(dataPkgRoles::selected):				return false;
	case int(dataPkgRoles::lines):					return DataSet::getDataSetViewLines(column>0, row>0, true, true);
	case int(dataPkgRoles::value):					return "";
	case int(dataPkgRoles::columnType):				return int(columnType::scale);
	default:										return QVariant();
	}

	return QVariant(); //gcc might complain some more I guess?
}

QVariant ExpandDataProxyModel::headerData(int section, Qt::Orientation orientation, int role) const
{
	if (!sourceModel() || role == -1) // Role not defined
		return QVariant();

	if (orientation == Qt::Orientation::Horizontal)
	{
		if (section < sourceModel()->columnCount())
			return sourceModel()->headerData(section, orientation, role);
		else
			switch(role)
			{
			case int(dataPkgRoles::columnIsComputed):				return false;
			case int(dataPkgRoles::computedColumnIsInvalidated):	return false;
			case int(dataPkgRoles::filter):							return false;
			case int(dataPkgRoles::computedColumnError):			return "";
			case int(dataPkgRoles::columnType):						return int(columnType::unknown);
			case int(dataPkgRoles::maxColString):					return "XXXXXXXXXXX";
			default:												return "";
			}
	}
	else if (orientation == Qt::Orientation::Vertical)
	{
		if (section < sourceModel()->rowCount())
			return sourceModel()->headerData(section, orientation, role);
		else if (section == 0 && role == int(dataPkgRoles::maxRowHeaderString))
			return "XXXX";
		else
			return  section + 1;
	}

	return QVariant();
}

DataSet * ExpandDataProxyModel::gridSourceDataSet() const
{
	GridModel * grid = qobject_cast<GridModel*>(sourceModel());
	if (!grid || !grid->dataSet() || !grid->dataSet()->isOpen())
		return nullptr;
	return grid->dataSet();
}

Qt::ItemFlags ExpandDataProxyModel::flags(const QModelIndex &index) const
{
	if (!sourceModel())
		return Qt::NoItemFlags;

	// The editing gate (data-edit-design §7): a LEGACY source (DataSetTableModel) or a LIVE
	// NEO dataset (the GridModel holding an open one) — both are editable surfaces; the
	// virtual area past the source is editable in expand mode exactly as legacy allowed
	// (an edit anchored beyond the extent GROWS the dataset: insert_block's design).
	const bool editableSource = dataSetSourceModel() != nullptr || gridSourceDataSet() != nullptr;

	if (index.column() < sourceModel()->columnCount() && index.row() < sourceModel()->rowCount())
	{
		Qt::ItemFlags sourceFlags = sourceModel()->flags(sourceModel()->index(index.row(), index.column()));
		return editableSource ? sourceFlags : (sourceFlags & ~Qt::ItemIsEditable);
	}

	return Qt::ItemIsSelectable | Qt::ItemIsEnabled | (editableSource ? Qt::ItemIsEditable : Qt::NoItemFlags);
}

QModelIndex ExpandDataProxyModel::index(int row, int column, const QModelIndex &) const
{
	if (!sourceModel())
		return QModelIndex();

	return createIndex(row, column);
}

QModelIndex ExpandDataProxyModel::parent(const QModelIndex &index) const
{
	return QModelIndex();
}


bool ExpandDataProxyModel::isRowVirtual(int row) const
{
	if (!sourceModel())
		return false;

	return row >= sourceModel()->rowCount();
}

bool ExpandDataProxyModel::isColumnVirtual(int col) const
{
	if (!sourceModel())
		return false;

	return col >= sourceModel()->columnCount();
}

int ExpandDataProxyModel::shownToRaw(int shownIndex, bool isRow) const
{
	QAbstractItemModel * src = sourceModel();
	if (!src)
		return shownIndex;

	DataSetTableModel * table = qobject_cast<DataSetTableModel *>(src);
	if (!table)
		return shownIndex;

	const int shownCount = isRow ? src->rowCount() : src->columnCount();

	if (shownIndex <= 0)
		shownIndex = 0;

	// Past the shown region (virtual/expand area): map to the raw slot the virtual cell will occupy
	// once the table is grown to include it (shown index "shownCount + k" sits at raw tail + k).
	if (shownIndex >= shownCount)
		return (isRow ? dataSetSourceModel()->rowCount() : dataSetSourceModel()->columnCount())
			+ (shownIndex - shownCount);

	QModelIndex shownIdx	= isRow ? table->index(shownIndex, 0) : table->index(0, shownIndex);
	QModelIndex raw			= table->mapToSource(shownIdx);

	if (raw.isValid())
		return isRow ? raw.row() : raw.column();

	return isRow ? dataSetSourceModel()->rowCount() : dataSetSourceModel()->columnCount();
}

std::vector<std::pair<int,int>> ExpandDataProxyModel::rawRunsFromShown(bool isRow, int shownStart, int shownCount) const
{
	std::vector<std::pair<int,int>> runs;

	if (!sourceModel())
		return runs;

	const int maxShown = isRow ? sourceModel()->rowCount() : sourceModel()->columnCount();

	int runStart = -1,
		lastRaw  = -1;

	for (int s = shownStart; s < shownStart + shownCount && s < maxShown; s++)
	{
		int r = shownToRaw(s, isRow);
		if (r < 0)
			continue;

		if (runStart < 0)
		{
			runStart = r;
			lastRaw  = r;
		}
		else if (r == lastRaw + 1)
			lastRaw = r;
		else
		{
			runs.push_back({runStart, lastRaw - runStart + 1});
			runStart = r;
			lastRaw  = r;
		}
	}

	if (runStart >= 0)
		runs.push_back({runStart, lastRaw - runStart + 1});

	return runs;
}

void ExpandDataProxyModel::removeRuns(bool isRows, const std::vector<std::pair<int,int>>& shownGroups)
{
	DataSet * ds = dataSetSourceModel();
	if (!ds)
		return;

	std::vector<std::pair<int,int>> rawRuns;
	for (const auto & startCount : shownGroups)
	{
		auto runs = rawRunsFromShown(isRows, startCount.first, startCount.second);
		rawRuns.insert(rawRuns.end(), runs.begin(), runs.end());
	}

	if (rawRuns.empty())
		return;

	// Sort ascending and merge adjacent raw runs (was only ever true if two shown groups touched).
	std::sort(rawRuns.begin(), rawRuns.end(), [](const auto & a, const auto & b){ return a.first < b.first; });

	std::vector<std::pair<int,int>> merged;
	for (const auto & run : rawRuns)
	{
		if (!merged.empty() && merged.back().first + merged.back().second == run.first)
			merged.back().second += run.second;
		else
			merged.push_back(run);
	}

	int total = 0;
	for (const auto & run : merged)
		total += run.second;

	UndoStack * stack = undoStack();
	stack->startMacro(isRows ? tr("Remove %1 rows").arg(total) : tr("Remove %1 columns").arg(total));

	// The excision, Cut 4: row/column removal was legacy-only (the lane rail has no
	// delete op yet — growth is remote, extent edits come with the derived-columns era).
	// The macro is ended empty, so this is an honest no-op.
	Q_UNUSED(merged);
	stack->endMacro();
}

void ExpandDataProxyModel::removeRows(int start, int count)
{
	if (!sourceModel() || count <= 0 || start < 0 || start >= sourceModel()->rowCount())
		return;

	if (start + count > sourceModel()->rowCount())
		count = sourceModel()->rowCount() - start;

	removeRuns(true, {{start, count}});
}

void ExpandDataProxyModel::removeRowGroups(std::vector<std::pair<int, int> > groups)
{
	removeRuns(true, groups);
}

void ExpandDataProxyModel::removeColumns(int start, int count)
{
	if (!sourceModel() || count <= 0 || start < 0 || start >= sourceModel()->columnCount())
		return;

	if (start + count > sourceModel()->columnCount())
		count = sourceModel()->columnCount() - start;

	removeRuns(false, {{start, count}});
}

void ExpandDataProxyModel::removeColumnGroups(std::vector<std::pair<int, int> > groups)
{
	removeRuns(false, groups);
}

void ExpandDataProxyModel::insertRows(int row, int count)
{
	Q_UNUSED(row);
	Q_UNUSED(count);
	// The excision, Cut 4: structural inserts were legacy-only; on lane the view grows
	// remotely (an insert_block past the extent) — no local resize exists.
}

void ExpandDataProxyModel::insertColumns(int col, int count)
{
	Q_UNUSED(col);
	Q_UNUSED(count);
}

void ExpandDataProxyModel::insertColumn(int col, bool computed, bool R)
{
	Q_UNUSED(col);
	Q_UNUSED(computed);
	Q_UNUSED(R);
	// Column creation on lane is ColumnModel's job (insertColsOp via the new-column
	// editor) — this legacy entry point is inert.
}

void ExpandDataProxyModel::resize(int row, int col, bool onlyExpand, const QString& undoText)
{
	Q_UNUSED(row);
	Q_UNUSED(col);
	Q_UNUSED(onlyExpand);
	Q_UNUSED(undoText);
	// The excision, Cut 4: local table resizing was legacy-only. On lane the dataset's
	// extent is the backend's (growth is a remote edit; revisions restart the view).
}

bool ExpandDataProxyModel::useUndoStack() const
{
	return sourceModel() != nullptr;
}

bool ExpandDataProxyModel::setData(const QModelIndex &index, const QVariant &value, int role)
{
	if (!sourceModel() || index.row() < 0 || index.column() < 0)
		return false;

	// NEO edit surface: ONE insert_block per commit boundary — the cell's escaped text is
	// the §1.2 tail, the anchor is the shown index (identity: the NEO view has no filter
	// compaction; an anchor past the extent GROWS the dataset remotely — data_changed
	// restarts the view, no local resize). Labels/roles stay the label editor's business.
	if (!dataSetSourceModel())
	{
		DataSet * ds = gridSourceDataSet();
		if (!ds)
		{
			Log::log() << "ExpandDataProxyModel::setData: no live dataset on the NEO source — edit refused (row " << index.row() << ", col " << index.column() << ")" << std::endl;
			return false;
		}

		Log::log() << "DataEdit: cell edit at (row " << index.row() << ", col " << index.column() << ") — one insert_block" << std::endl;

		const QString cell = DataEdit::escapeCell(value);
		undoStack()->endMacro(new DataEditCommand(
			ds,
			DataEdit::insertBlockOp(uint64_t(index.row()), uint64_t(index.column())),
			DataEdit::tsvFromCells({ { cell } }),
			tr("Edit cell")));
		return true;
	}

	// The excision, Cut 4: the legacy tail (local resize + SetDataCommand) is gone — the
	// NEO edit surface above is the only route. The legacy source arm (DataSetTableModel)
	// no longer edits.
	return false;
}

void ExpandDataProxyModel::pasteSpreadsheet(int row, int col, const std::vector<std::vector<QString>> & values, const std::vector<std::vector<QString>> & labels, const QStringList & colNames, const std::vector<boolvec> & selected)
{
	if (!sourceModel() || row < 0 || col < 0 || values.size() == 0 || values[0].size() == 0 )
		return;

	DataSet * ds = dataSetSourceModel();
	if (!ds)
	{
		// NEO paste: ONE insert_block over the whole rectangle (the commit-boundary rule —
		// one wire trip, one revision bump, one undo entry). Identity mapping (no filter
		// compaction in the NEO view); an anchor past the extent grows the dataset, holes
		// null. Cells escape once, here, per §1.2. Labels/colNames are the label editor's
		// and the header-rename surfaces' business — not this op.
		DataSet * grid = gridSourceDataSet();
		if (!grid)
		{
			Log::log() << "ExpandDataProxyModel::pasteSpreadsheet: no live dataset on the NEO source — paste dropped" << std::endl;
			return;
		}

		std::vector<std::vector<QString>> escaped(values.size());
		for (size_t c = 0; c < values.size(); ++c)
		{
			escaped[c].reserve(values[c].size());
			for (size_t r = 0; r < values[c].size(); ++r)
				escaped[c].push_back(DataEdit::escapeCell(QVariant(values[c][r])));
		}

		undoStack()->endMacro(new DataEditCommand(
			grid,
			DataEdit::insertBlockOp(uint64_t(row), uint64_t(col)),
			DataEdit::tsvFromCells(escaped),
			tr("Paste %1×%2").arg(values.size()).arg(values[0].size())));
		return;
	}

	// The excision, Cut 4: the legacy paste (raw-table mapping + PasteSpreadsheetCommand)
	// is gone — lane pastes take the insert_block path above; there is no other route.
}


// The excision, Cut 4: columnIndexesToNames died with its last caller (the legacy
// retype/reverse/toggle commands).

int ExpandDataProxyModel::setColumnType(intset columnIndexes, int columnType)
{
	// NEO: a retype is a schema_change (§3 — the column set never moves; the lane
	// re-encodes the data, coerce-or-error). The shown indexes ARE the schema indexes
	// (no filter compaction in the NEO view); names come from the schema, not the legacy
	// mirror (renames keep the mirror stale — the grid reads the schema).
	if (!dataSetSourceModel())
	{
		DataSet * ds = gridSourceDataSet();
		if (!ds)
		{
			Log::log() << "ExpandDataProxyModel::setColumnType: no live NEO dataset — ignored" << std::endl;
			return columnType;
		}

		std::set<std::string> names;
		for (int col : columnIndexes)
		{
			const ColumnInfo * info = col >= 0 ? ds->schemaColumnAt(size_t(col)) : nullptr;
			if (info)
				names.insert(info->name);
		}
		if (!names.empty())
			undoStack()->endMacro(new DataEditCommand(
				ds,
				DataEdit::schemaChangeTypeOp(ds, names, static_cast<enum columnType>(columnType)),
				QByteArray(),
				tr("Change column type")));
		return columnType;
	}

	// The excision, Cut 4: the legacy retype command is gone; lane retypes take the
	// schema_change path above and nothing else reaches here.
	return columnType;
}

void ExpandDataProxyModel::columnReverseValues(intset columnIndexes)
{
	Q_UNUSED(columnIndexes);
	// The excision, Cut 4: value/label reversal was legacy-only (returns with the labels
	// editor, B2).
}

void ExpandDataProxyModel::columnautoSortByValues(intset columnIndexes)
{
	Q_UNUSED(columnIndexes);
}

void ExpandDataProxyModel::copyColumns(int startCol, const std::vector<Json::Value>& copiedColumns)
{
	if (!sourceModel() || startCol < 0 || copiedColumns.size() == 0)
		return;

	// The excision, Cut 4: column copying was legacy-only (it cloned Column serializations
	// into the raw table); NEO column creation is the new-column editor's insertColsOp.
	Log::log() << "ExpandDataProxyModel::copyColumns: legacy column-copy is gone — ignored" << std::endl;
}

Json::Value ExpandDataProxyModel::serializedColumn(int col)
{
	DataSet * ds = dataSetSourceModel();
	if (!ds)
		return Json::nullValue;

	int rawCol = shownToRaw(col, false);
	if (rawCol >= 0 && rawCol < ds->columnCount())
		return ds->column(rawCol)->serialize();

	return Json::nullValue;
}

DataSet	* ExpandDataProxyModel::dataSetSourceModel() const 
{ 
	DataSetTableModel * table = qobject_cast<DataSetTableModel*>(sourceModel()); 
	
	if(table)
		return table->dataSetSourceModel();
	
	return nullptr;
}
