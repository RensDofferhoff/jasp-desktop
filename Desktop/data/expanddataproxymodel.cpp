#include "expanddataproxymodel.h"
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

	// The editing gate (data-edit-design §7): the LIVE NEO dataset (the GridModel holding an
	// open one). The excision, Cut 5: the legacy source arm is gone.
	const bool editableSource = gridSourceDataSet() != nullptr;

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
	Q_UNUSED(isRow);
	// The excision, Cut 5: the legacy filter-compaction mapping (DataSetTableModel) is gone
	// — the NEO view has no filter compaction, so shown == raw, identity.
	return shownIndex;
}

// The excision, Cut 5: rawRunsFromShown mapped shown runs through the legacy filter
// compaction — the NEO view is identity (shownToRaw). Died with removeRuns' command body.

void ExpandDataProxyModel::removeRuns(bool isRows, const std::vector<std::pair<int,int>>& shownGroups)
{
	// The excision, Cut 5: structural removal was legacy-only and its commands died in
	// Cut 4; the lane rail has no delete op yet. Honest no-op.
	Q_UNUSED(isRows);
	Q_UNUSED(shownGroups);
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
	// The excision, Cut 5: the legacy source arm is gone — this is the only surface.
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

	// The excision, Cut 5: the NEO paste is the only route (the legacy source arm is gone).
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
	// The excision, Cut 5: the legacy source arm is gone — this is the only route.
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

// The excision, Cut 5: serializedColumn read legacy Column storage (Column::serialize)
// through the DataSetTableModel arm — both gone. dataSetSourceModel() (the legacy-arm
// detector) died with them.
