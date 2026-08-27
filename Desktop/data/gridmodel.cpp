#include "gridmodel.h"

#include <QLocale>
#include <algorithm>
#include <limits>

#include "datamodel.h"
#include "datasetregistry.h"
#include "viewfiller.h"
#include "data/datasetpackage.h"
#include "utilities/qutils.h"
#include "jasptheme.h"
#include "log.h"

const QString	GridModel::placeholderText = QStringLiteral("…");	// U+2026 — distinct from empty cells and nulls

GridModel::GridModel(QObject * parent)
	: QAbstractTableModel(parent)
	, _registry(DataSetPackage::pkg()->registry())
{
	connect(_registry, &DatasetRegistry::activeChanged, this, &GridModel::bindToActive);
	bindToActive();		// startup: no active dataset yet → empty 0×0 model
}

void GridModel::bindToActive()
{
	beginResetModel();

	if (_buffer)
		disconnect(_buffer, nullptr, this, nullptr);
	if (_filler)
		disconnect(_filler, nullptr, this, nullptr);

	_model	= _registry->active();
	_buffer	= _registry->viewBuffer();
	_filler	= _registry->viewFiller();

	_cacheRow = UINT64_MAX;
	_cacheCol = -1;
	_cacheText.clear();
	_cacheNull = false;
	_cacheMissing = false;
	_viewStatus.clear();

	if (_buffer)
	{
		connect(_buffer, &DataViewBuffer::chunkIngested,	this, &GridModel::onChunkIngested);
		connect(_buffer, &DataViewBuffer::chunksEvicted,	this, &GridModel::onChunksEvicted);
		connect(_buffer, &DataViewBuffer::bufferReset,		this, &GridModel::onBufferReset);
	}
	if (_filler)
	{
		connect(_filler, &ViewFiller::budgetReached,	this, &GridModel::onFillBudgetReached);
		connect(_filler, &ViewFiller::fillCompleted,	this, &GridModel::onFillCompleted);
		connect(_filler, &ViewFiller::fillFailed,		this, &GridModel::onFillFailed);
		connect(_filler, &ViewFiller::fillRecovered,	this, &GridModel::onFillRecovered);
	}

	endResetModel();
	emit viewStatusChanged(_viewStatus);
}

void GridModel::onChunkIngested(quint64 firstRow, quint64 rows)
{
	// Sliding mode: the model already spans the whole dataset (placeholders for the not-yet-
	// resident rows), so a chunk landing only changes CELL CONTENT of its row range — never the
	// row count. The view's dataChanged path (cacheItems: true → calculateCellSizes → full
	// viewport rebuild from fresh model data) is the exact machinery the per-chunk
	// row-insertions rode in increment 2 — verified against the real GUI.
	refreshRows(firstRow, rows);
}

void GridModel::onChunksEvicted(quint64 firstRow, quint64 rows)
{
	// The chunk's rows fall back to placeholder rendering (format doc §2.5: eviction drops the
	// split-row cache entries with it; a re-fetch restores the content idempotently).
	refreshRows(firstRow, rows);
}

void GridModel::refreshRows(quint64 firstRow, quint64 rows)
{
	const int cols	= columnCount();
	const int total	= rowCount(QModelIndex());	// (params named to avoid shadowing this)
	if (rows == 0 || cols == 0 || firstRow >= quint64(total))
		return;
	const int first	= int(firstRow);
	const int last	= int(std::min<quint64>(firstRow + rows, quint64(total))) - 1;
	// QUEUED emission (matches inc2's proven delivery semantics: chunk arrivals then rode
	// rowsInserted, which the view connects Qt::QueuedConnection — datasetviewbase.cpp L81).
	// Direct emission runs the whole viewport rebuild synchronously inside the arrival
	// callback, nested under the view's mid-mutation item pools and the viewport-signal chain
	// — found against the real GUI: jump-target cells stayed "…" despite the chunk landing.
	QMetaObject::invokeMethod(this, [this, first, last, cols]()
	{
		emit dataChanged(index(first, 0), index(last, cols - 1), { Qt::DisplayRole });
	}, Qt::QueuedConnection);
}

void GridModel::onBufferReset()
{
	beginResetModel();
	_cacheRow = UINT64_MAX;
	_cacheCol = -1;
	_cacheText.clear();
	_cacheNull = false;
	endResetModel();
}

void GridModel::onFillBudgetReached(quint64 rowsResident, quint64 rowsTotal)
{
	// Sliding-mode stop state (format doc §2.5): the background fill is at budget and the
	// viewport is served — the rest of the dataset stays reachable (placeholders load on
	// demand; misses evict the farthest chunks). Replaces increment 2's buffered-prefix note.
	const QString status = tr("Large dataset — %1 of %2 rows buffered; the rest loads as you scroll.")
		.arg(QLocale::system().toString(qulonglong(rowsResident)))
		.arg(QLocale::system().toString(qulonglong(rowsTotal)));
	if (_viewStatus == status)
		return;	// budgetReached re-fires after every evict+fetch cycle — emit only on change
	_viewStatus = status;
	emit viewStatusChanged(_viewStatus);
}

void GridModel::onFillCompleted()
{
	if (!_viewStatus.isEmpty())
	{
		_viewStatus.clear();
		emit viewStatusChanged(_viewStatus);
	}
}

void GridModel::onFillFailed(QString message)
{
	const QString status = tr("Data view incomplete: %1").arg(message);
	if (_viewStatus == status)
		return;	// retried failures re-emit — only update on change
	_viewStatus = status;
	emit viewStatusChanged(_viewStatus);
}

void GridModel::onFillRecovered()
{
	// A chunk landed after a failure — the error note is stale, data is flowing again. (The
	// budget note, if it follows, re-emits on the next budget-stop with fresh numbers.)
	if (_viewStatus.isEmpty())
		return;
	_viewStatus.clear();
	emit viewStatusChanged(_viewStatus);
}

int GridModel::rowCount(const QModelIndex & parent) const
{
	if (parent.isValid())
		return 0;	// flat table
	// Sliding mode: the model spans the WHOLE dataset — non-resident rows render as
	// placeholders until their chunk arrives (or after eviction). Item creation in
	// DataSetViewBase is viewport-driven, so a multi-million-row span costs nothing but the
	// scrollbar. int-capped for the view's index machinery.
	if (!_model)
		return 0;
	return int(std::min<uint64_t>(_model->rows(), uint64_t(std::numeric_limits<int>::max())));
}

int GridModel::columnCount(const QModelIndex & parent) const
{
	if (parent.isValid())
		return 0;
	return _model ? int(_model->columnCount()) : 0;
}

void GridModel::ensureCell(int row, int col) const
{
	if (_cacheRow == uint64_t(row) && _cacheCol == col)
		return;
	_cacheRow	= uint64_t(row);
	_cacheCol	= col;
	_cacheText.clear();
	_cacheNull	= false;
	_cacheMissing = false;
	if (_buffer)
		_cacheMissing = !_buffer->cellAt(uint64_t(row), col, _cacheText, _cacheNull);
}

QVariant GridModel::data(const QModelIndex & index, int role) const
{
	if (!_buffer || !index.isValid() || role == -1)
		return QVariant();

	const int row = index.row(),
			  col = index.column();

	if (row < 0 || col < 0 || row >= rowCount() || col >= columnCount())
		return QVariant();

	switch (role)
	{
	// The cell string serves all value-flavoured roles (v1: values only; labels are a later
	// overlay). The buffer's bytes are the lane's locale-rendered display strings — the
	// frontend does no numeric formatting (data-view-format.md §1.2 division of labor).
	// Non-resident rows render the placeholder (sliding mode, format doc §2.5) — visually
	// distinct from empty cells and nulls, which both render empty (legacy parity).
	case Qt::DisplayRole:
	case int(dataPkgRoles::noSepaDisplay):
	case int(dataPkgRoles::label):
	case int(dataPkgRoles::value):
	case int(dataPkgRoles::valueLabelPair):
		ensureCell(row, col);
		if (_cacheMissing)	return placeholderText;
		return _cacheNull ? QString() : _cacheText;	// null renders as an empty cell (legacy parity)

	case int(dataPkgRoles::shadowDisplay):
		return QString();

	case int(dataPkgRoles::filter):
		return true;									// nothing filtered out yet

	case int(dataPkgRoles::selected):
		return false;

	case int(dataPkgRoles::lines):
		return DataSetPackage::getDataSetViewLines(col > 0, row > 0, true, true);

	case int(dataPkgRoles::columnType):
	{
		const ColumnInfo * c = _model ? _model->columnAt(size_t(col)) : nullptr;
		return int(c ? c->type : columnType::unknown);
	}
	case int(dataPkgRoles::name):
	{
		const ColumnInfo * c = _model ? _model->columnAt(size_t(col)) : nullptr;
		return c ? tq(c->displayName) : QString();
	}
	case int(dataPkgRoles::description):
	{
		const ColumnInfo * c = _model ? _model->columnAt(size_t(col)) : nullptr;
		return c ? tq(c->description) : QString();
	}
	case int(dataPkgRoles::columnPkgIndex):
		return col;

	case int(dataPkgRoles::computedColumnType):
		return int(computedColumnType::notComputed);

	default:
		return QVariant();
	}
}

QVariant GridModel::headerData(int section, Qt::Orientation orientation, int role) const
{
	if (role == -1)
		return QVariant();

	if (orientation == Qt::Horizontal)
	{
		const ColumnInfo * col = _model ? _model->columnAt(size_t(section)) : nullptr;
		if (!col)
			return QVariant();

		switch (role)
		{
		case Qt::DisplayRole:
		case int(dataPkgRoles::title):
		case int(dataPkgRoles::name):
			return tq(col->displayName);

		case int(dataPkgRoles::description):
			return tq(col->description);

		case int(dataPkgRoles::columnType):
			return int(col->type);

		case int(dataPkgRoles::columnIsComputed):
		case int(dataPkgRoles::computedColumnIsInvalidated):
		case int(dataPkgRoles::filter):
		case int(dataPkgRoles::labelsHasFilter):
		case int(dataPkgRoles::inEasyFilter):
			return false;

		case int(dataPkgRoles::computedColumnError):
			return QString();

		case int(dataPkgRoles::computedColumnType):
			return int(computedColumnType::notComputed);

		case int(dataPkgRoles::columnPkgIndex):
			return section;

		// maxColString deliberately NOT served: the view falls back to columnWidthFallback,
		// which is O(1) — the view asks headerData for EVERY column, so an O(rows) max-string
		// scan in the frontend is off the table (design §7.2; a lane-computed max_width per
		// column is the later nicety).
		case int(dataPkgRoles::columnWidthFallback):
			return columnWidthFallbackFor(col->type);

		default:
			return QVariant();
		}
	}

	// Vertical: row numbers.
	if (role == Qt::DisplayRole)
		return section + 1;
	if (role == int(dataPkgRoles::maxRowHeaderString))
		return QString::number(_model ? _model->rows() : 0);
	return QVariant();
}

qreal GridModel::columnWidthFallbackFor(columnType type)
{
	// Fixed width per type; the view subtracts its own horizontal padding from this.
	const char * rep = "??????";
	switch (type)
	{
	case columnType::scale:		rep = "1234567,89";	break;
	case columnType::ordinal:
	case columnType::nominal:	rep = "level value";	break;
	default: break;
	}
	return JaspTheme::fontMetrics().size(Qt::TextSingleLine, QString(rep)).width() + 16;
}

Qt::ItemFlags GridModel::flags(const QModelIndex & index) const
{
	if (!index.isValid())
		return Qt::NoItemFlags;
	// Read-only increment (design §7.5): selectable + enabled, NEVER editable — the editing
	// surface returns with data_edit.
	return Qt::ItemIsSelectable | Qt::ItemIsEnabled;
}

bool GridModel::setData(const QModelIndex &, const QVariant &, int)
{
	return false;	// read-only until data_edit lands
}

QHash<int, QByteArray> GridModel::roleNames() const
{
	// Same contract as DataSetPackage::roleNames(): the base roles + every dataPkgRoles name —
	// DataSetViewBase resolves roles through the model's roleNames map. (The static is seeded
	// from the base implementation on first call — a member call, evaluated in `this` context.)
	static bool						set = false;
	static QHash<int, QByteArray>	roles = QAbstractItemModel::roleNames();

	if(!set)
	{
		for (const auto & roleAndName : dataPkgRolesToStringMap())
			roles[int(roleAndName.first)] = QByteArray::fromStdString(roleAndName.second);
		set = true;
	}

	return roles;
}

QString GridModel::columnName(int column) const
{
	const ColumnInfo * c = _model ? _model->columnAt(size_t(column)) : nullptr;
	return c ? tq(c->displayName) : QString();
}

void GridModel::setColumnName(int, QString)
{
	// Read-only v1: renames return with data_edit (design §7.3).
}

QVariant GridModel::getColumnTypesWithIcons() const
{
	// Pure type→icon metadata — identical for NEO datasets.
	return DataSetPackage::pkg()->getColumnTypesWithIcons();
}

bool GridModel::columnUsedInEasyFilter(int) const
{
	return false;	// no filters in v1
}

void GridModel::resetAllFilters()
{
	// no-op until filters land
}

bool GridModel::isColumnNameFree(QString name) const
{
	return _model ? _model->isColumnNameFree(fq(name)) : true;
}

void GridModel::setShowInactive(bool showInactive)
{
	if (_showInactive == showInactive)
		return;
	_showInactive = showInactive;
	emit showInactiveChanged(showInactive);
}
