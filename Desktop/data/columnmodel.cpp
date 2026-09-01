#include "timers.h"
#include "qutils.h"
#include "column.h"
#include "dataenums.h"
#include "dataset.h"
#include "jasptheme.h"
#include "columnmodel.h"
#include "columnutils.h"
#include "datasetpackage.h"
#include "jaspclient/dataedit.h"
#include "log.h"
#include "gui/preferencesmodel.h"

ColumnModel::ColumnModel() : QIdentityProxyModel(DataSetPackage::pkg())
{
	connect(DataSetPackage::pkg(),	&DataSetPackage::shownFilterChanged,			this, &ColumnModel::refreshFilteredOut				);

	connect(DataSetPackage::pkg(),	&DataSetPackage::allFiltersReset,				this, &ColumnModel::allFiltersReset				);
	
	connect(DataSetPackage::pkg(),	&DataSetPackage::datasetChanged,				this, &ColumnModel::checkCurrentColumn			);
	connect(DataSetPackage::pkg(),	&DataSetPackage::workspaceEmptyValuesChanged,	this, &ColumnModel::emptyValuesChanged			);
	connect(DataSetPackage::pkg(),	&DataSetPackage::chooseColumn,					this, &ColumnModel::setChosenColumn				);
	connect(DataSetPackage::pkg(),	&DataSetPackage::shownDataSetChanged,			this, &ColumnModel::shownDataSetChangedHandler	);
}

QVariant ColumnModel::columnTypeFriendlyMapping(computedColumnType compColT)
{
	typedef QMap<QString, QVariant> localMap;
	
	return	localMap(
			{	
				std::make_pair("value", computedColumnTypeToQString(compColT)),		
				std::make_pair("label", Column::columnTypeFriendlyName(compColT))	
			});
}

QVariantList ColumnModel::computedTypeValues() const
{
	switch(column() ? column()->codeType() : computedColumnType::notComputed)
	{
	case computedColumnType::notComputed:
	case computedColumnType::rCode:
	case computedColumnType::constructorCode:
		return { columnTypeFriendlyMapping(computedColumnType::notComputed), columnTypeFriendlyMapping(computedColumnType::rCode), columnTypeFriendlyMapping(computedColumnType::constructorCode) };

	case computedColumnType::analysis:
		return  { columnTypeFriendlyMapping(computedColumnType::analysis) };

	case computedColumnType::analysisNotComputed:
		return { columnTypeFriendlyMapping(computedColumnType::analysisNotComputed), columnTypeFriendlyMapping(computedColumnType::notComputed), columnTypeFriendlyMapping(computedColumnType::rCode), columnTypeFriendlyMapping(computedColumnType::constructorCode) };;
	}
	
	return {};
}

QVariantList ColumnModel::columnTypeValues() const
{
	typedef QMap<QString, QVariant> localMap;
	
	return {
		localMap({ std::make_pair("value", columnTypeToQString(columnType::scale)),				std::make_pair("label", QObject::tr("Scale")),		std::make_pair("columnTypeIcon", JaspTheme::currentIconPath() + "variable-scale.svg")	}),
		localMap({ std::make_pair("value", columnTypeToQString(columnType::ordinal)),			std::make_pair("label", QObject::tr("Ordinal")),	std::make_pair("columnTypeIcon", JaspTheme::currentIconPath() + "variable-ordinal.svg")	}),
		localMap({ std::make_pair("value", columnTypeToQString(columnType::nominal)),			std::make_pair("label", QObject::tr("Nominal")),	std::make_pair("columnTypeIcon", JaspTheme::currentIconPath() + "variable-nominal.svg")	})
	};
}

QString ColumnModel::columnNameQ()
{
	if (_virtual) return _dummyColumn.name;

	// NEO adapter: the SCHEMA is the name truth — the mirror never renames, so its name
	// goes stale the moment a NEO rename lands (the "edit didn't stick" disease).
	if (const ColumnInfo * info = laneSchemaColumn())
		return QString::fromStdString(info->name);

	return QString::fromStdString(column() ? column()->name() : "");
}


void ColumnModel::setColumnNameQ(QString newColumnName)
{
	if (_beingRefreshed || newColumnName == columnNameQ()) return;

	// NEO (R2 write-routing): the commit is a wire op, never a legacy Column command — the
	// mirror is grow-only metadata; a rename there would silently diverge from the lane
	// schema the grid renders. The virtual branch is the worst legacy offender (it inserts
	// ghost Columns with no null guard at all): one insert_cols replaces the whole macro.
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		if (_virtual)
		{
			// The editor's position, clamped to the schema extent (a -1 index appends).
			const uint64_t at = (_columnIndex >= 0 && size_t(_columnIndex) <= neoDataSet->schema().size())
								? uint64_t(_columnIndex) : uint64_t(neoDataSet->schema().size());
			undoStack()->endMacro(new DataEditCommand(
				neoDataSet,
				DataEdit::insertColsOp(at, fq(newColumnName), _dummyColumn.type),
				QByteArray(),
				tr("Insert column")));
		}
		else
		{
			// The chosen column by SCHEMA identity: the lane mirror is grown in schema order
			// but never RENAMED, so its names go stale after prior NEO renames — the index is
			// the stable correspondence (chosenColumn() is the mirror index).
			const int idx = chosenColumn();
			const ColumnInfo * info = idx >= 0 ? neoDataSet->schemaColumnAt(size_t(idx)) : nullptr;
			if (info)
				undoStack()->endMacro(new DataEditCommand(
					neoDataSet,
					DataEdit::schemaChangeRenameOp(neoDataSet, info->name, fq(newColumnName)),
					QByteArray(),
					tr("Rename column")));
			else
				Log::log() << "ColumnModel::setColumnNameQ: chosen column not found in the lane schema — rename refused" << std::endl;
		}
		return;
	}

	if (_virtual)
	{
		// Legacy insert-at-index flow — lane datasets are handled above (insertColsOp /
		// schemaChangeRenameOp and return). Nothing to do here anymore.
	}
}

QString ColumnModel::columnTitle() const
{
	if (_virtual) return _dummyColumn.title;

	// NEO adapter: display_name IS the "Long name" (P4) — the schema is its truth.
	if (const ColumnInfo * info = laneSchemaColumn())
		return QString::fromStdString(info->displayName);

	return QString::fromStdString(column() ? column()->title() : "");
}

void ColumnModel::setColumnTitle(const QString & newColumnTitle)
{
	if (_beingRefreshed)
		return;

	// NEO: "Long name" IS the wire display_name — the same rename gesture as the Name
	// field (P4 couples them: declaring display_name derives the field name).
	// NEO: "Long name" IS the wire display_name; declaring it renames (P4 couples the
	// field name's derivation to it). Same op as the Name field — one gesture. The
	// equality guard matters here: the switch-time force-commit and focus-out handlers can
	// both re-fire with unchanged text, and a no-guard submit would mint a spurious
	// revision (and undo entry) on every such event.
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		if (!_virtual && !newColumnTitle.isEmpty())
		{
			const int idx = chosenColumn();
			const ColumnInfo * info = idx >= 0 ? neoDataSet->schemaColumnAt(size_t(idx)) : nullptr;
			if (info && info->displayName != fq(newColumnTitle))
				undoStack()->endMacro(new DataEditCommand(
					neoDataSet,
					DataEdit::schemaChangeRenameOp(neoDataSet, info->name, fq(newColumnTitle)),
					QByteArray(),
					tr("Rename column")));
			else
				Log::log() << "ColumnModel::setColumnTitle: chosen column not found in the lane schema — rename refused" << std::endl;
		}
		return;
	}

	if (_virtual)
		_dummyColumn.title = newColumnTitle;
}

void ColumnModel::setDropLevels(QString dropLevels)
{
	if (_beingRefreshed)
		return;

	// NEO gate: the lane dictionary never prunes (≡ keep, by engine invariant), so the
	// drop/keep distinction has no lane meaning until data goes to R (analyses era).
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		Log::log() << "ColumnModel::setDropLevels: not applicable to lane datasets (the dictionary never prunes) — ignored" << std::endl;
		return;
	}

	dropLevelsType dropEm = dropLevelsType::drop;
	
	try { dropEm = dropLevelsTypeFromQString(dropLevels); } catch(...){}
	Q_UNUSED(dropEm); // only the legacy route consumed it — that route is gone (Cut 4)
}

QString ColumnModel::columnDescription() const
{
	if (_virtual) return _dummyColumn.description;

	// NEO adapter: honest "" — the wire carries no description yet (it lands with the
	// labels editor as jasp:description field metadata).
	if (const ColumnInfo * info = laneSchemaColumn())
		return tq(info->description);

	return tq(column() ? column()->description() : "");
}

QString ColumnModel::computeFilter() const
{
	if (_virtual) 
		return _dummyColumn.computeFilter;
	
	if(column())
		return tq(column()->computeFilter());
	
	return "";
}


bool ColumnModel::autoSort() const
{
	if (_virtual) 
		return PreferencesModel::prefs()->orderByValueByDefault();
	
	return column() && column()->autoSortByValue();
}

void ColumnModel::setAutoSort(bool newAutoSort)
{
	if (!column() || column()->autoSortByValue() == newAutoSort)
		return;
	
	column()->setAutoSortByValue(newAutoSort);
	
	emit autoSortChanged();
}

bool ColumnModel::useCustomEmptyValues() const
{
	if (_virtual || !column()) return false;

	return column()->hasCustomEmptyValues();
}

void ColumnModel::setUseCustomEmptyValues(bool useCustom)
{
	// The excision, Cut 4: the empty-values command family is gone with the legacy data
	// route (empty values are a legacy loading concept). `column()` is always null on
	// lane data, so this was already unreachable there — now it is honestly empty.
	Q_UNUSED(useCustom);
}

QStringList ColumnModel::emptyValues() const
{
	return (_virtual || !column()) ? QStringList() : tql(column()->emptyValues()->emptyStringsColumnModel());
}

int ColumnModel::rowsTotal() const
{
	return rowCount();	
}

int ColumnModel::rowCount(const QModelIndex &parent) const
{
	//The label-editor (and rowsTotal) present the column's *labels*, not its data rows. A (scale)
	//column's data lives in _dbls but only its non-empty labels are editable here, so the number of
	//rows a view should show is the number of labels, not the underlying row count.
	return parent.isValid() ? 0 : (column() ? int(column()->labelsNonEmptyCount()) : 0);
}

QString ColumnModel::dropLevels() const
{
	return dropLevelsTypeToQString(_virtual || !column() || column()->dropLevels() == dropLevelsType::noChoice ? dropLevelsType::drop : column()->dropLevels());
}

bool ColumnModel::hasSeveralNumericValues() const
{
	if(!column())
		return false;
	
	int numberOfNumericalValues = 0;
	for(Label * label : column()->labels())	
		if(!label->isEmptyValue())
		{
			static double dummy;
			
			if(label->originalValue().isDouble() && ColumnUtils::getDoubleValue(label->originalValueAsString(), dummy))
				numberOfNumericalValues++;

			if (numberOfNumericalValues > 1)
				return true;
		}
	
	return false;
}

void ColumnModel::setCustomEmptyValues(const QStringList& customEmptyValues)
{
	// The excision, Cut 4: see setUseCustomEmptyValues — empty values return with a
	// NEO-era design.
	Q_UNUSED(customEmptyValues);
}


void ColumnModel::addEmptyValue(const QString & value)
{
	QStringList values = emptyValues();
	values.push_back(value);
	setCustomEmptyValues(values);
}

void ColumnModel::removeEmptyValue(const QString & value)
{
	QStringList values = emptyValues();
	values.removeAll(value);
	setCustomEmptyValues(values);
}

void ColumnModel::resetEmptyValues()
{
	if(column())
		setCustomEmptyValues(tql(column()->data()->emptyValuesAsStrings()));
}

UndoStack *ColumnModel::undoStack()
{
	return UndoStack::singleton();
}


QVariantList ColumnModel::tabs() const
{
	QVariantList tabs;
	Column* col = column();
	
	if(_compactMode)
		tabs.push_back(QMap<QString, QVariant>({  std::make_pair("name", "basicInfo"), std::make_pair("title", tr("Column definition"))}));
	
	if(col)
	{
		if (col->isComputed() && (col->codeType() == computedColumnType::rCode || col->codeType() == computedColumnType::constructorCode))
			tabs.push_back(QMap<QString, QVariant>({  std::make_pair("name", "computed"), std::make_pair("title", tr("Computed column definition"))}));

		tabs.push_back(QMap<QString, QVariant>({  std::make_pair("name", "label"), std::make_pair("title", tr("Label editor"))}));
	}

	QMap<QString, QVariant> misingValues =	{  std::make_pair("name", "missingValues"), std::make_pair("title", tr("Missing values"))};
	tabs.push_back(misingValues);

	return tabs;
}


QString ColumnModel::currentColumnType() const
{
	if (_virtual) return columnTypeToQString(_dummyColumn.type);

	// NEO adapter: the schema's type (the mirror's goes stale after a NEO retype).
	if (const ColumnInfo * info = laneSchemaColumn())
		return columnTypeToQString(info->type);

	columnType type = column() ? column()->type() : columnType::scale;

	return columnTypeToQString(type);
}

QString ColumnModel::computedType() const
{
	if (_virtual) return computedColumnTypeToQString(_dummyColumn.computedType);

	return column() ? computedColumnTypeToQString(column()->codeType()) : computedColumnTypeToQString(computedColumnType::notComputed);
}

bool ColumnModel::computedTypeEditable() const
{
	if(_virtual)
		return true;

	if (!column())
		return false;

	switch (column()->codeType())
	{
	case computedColumnType::notComputed:
	case computedColumnType::analysisNotComputed:
	case computedColumnType::constructorCode:
	case computedColumnType::rCode:
		return true;

	default:
		return false;
	}
}

bool ColumnModel::isComputed() const
{
	if(_virtual)
		return false;

	if (!column())
		return false;

	return column()->isComputed();
}

void ColumnModel::setColumnDescription(const QString & newColumnDescription)
{
	if (_beingRefreshed)
		return;

	// NEO gate: description has no wire field yet (the lane never emits it; schema_change
	// has no entry key). Support lands with the labels-editor slice as jasp:description
	// field metadata — the same family as jasp:labels.
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		Log::log() << "ColumnModel::setColumnDescription: description not yet on the lane wire — ignored (jasp:description lands with the labels editor)" << std::endl;
		return;
	}

	if (_virtual)
		_dummyColumn.description = newColumnDescription;
}

void ColumnModel::setComputedType(QString type)
{
	if (_beingRefreshed || type.isEmpty() || type == computedType() || !computedColumnTypeValidName(fq(type)))
		return;

	computedColumnType cType = computedColumnTypeFromString(type.toStdString());

	// NEO gate: computed columns on lane data are a future era (no analyses run on lane
	// data yet).
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		Log::log() << "ColumnModel::setComputedType: computed columns are not yet available on lane datasets — ignored" << std::endl;
		return;
	}

	if (_virtual)
		_dummyColumn.computedType = cType;

	// The excision, Cut 4: the legacy computed-column command is gone; computed columns
	// return as derivations (ChangeKind::derived).

	emit tabsChanged();
}

void ColumnModel::setComputeFilter(const QString &newComputeFilter)
{
	if(_beingRefreshed || !column() || column()->computeFilter() == fq(newComputeFilter))
		return;

	// NEO gate: computed columns on lane data are a future era.
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		Log::log() << "ColumnModel::setComputeFilter: computed columns are not yet available on lane datasets — ignored" << std::endl;
		return;
	}

	if (_virtual)
		_dummyColumn.computeFilter = newComputeFilter;
	
	// The excision, Cut 4: the legacy compute-filter command is gone; computed columns
	// return as derivations (ChangeKind::derived).

	emit tabsChanged();
}

void ColumnModel::setColumnType(QString type)
{
	if (_beingRefreshed || type.isEmpty() || type == currentColumnType() || !columnTypeValidName(fq(type))) 
		return;

	columnType cType = columnTypeFromString(type.toStdString());

	// NEO (R2 write-routing): the variables-window dropdown is the THIRD type-switching
	// surface (the header menu + status-bar toggle route through the proxy since e2) —
	// same schema_change op, so all three surfaces agree on lane datasets.
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		if (!_virtual && !DataEdit::wireTypeOf(cType).isEmpty())
		{
			const int idx = chosenColumn();
			const ColumnInfo * info = idx >= 0 ? neoDataSet->schemaColumnAt(size_t(idx)) : nullptr;
			if (info)
				undoStack()->endMacro(new DataEditCommand(
					neoDataSet,
					DataEdit::schemaChangeTypeOp(neoDataSet, { info->name }, cType),
					QByteArray(),
					tr("Change column type")));
			else
				Log::log() << "ColumnModel::setColumnType: chosen column not found in the lane schema — retype refused" << std::endl;
		}
		return;
	}

	if (_virtual)
		_dummyColumn.type = cType;

	// The excision, Cut 4: the legacy retype command is gone; lane retypes route through
	// the schema_change op above.
}

std::vector<size_t> ColumnModel::getSortedSelection() const
{
	if (_virtual) return {};

	std::map<QString, size_t> mapValueToRow;

	for(size_t r=0; r<size_t(rowCount()); r++)
		mapValueToRow[data(index(r, 0), int(dataPkgRoles::value)).toString()] = r;

	std::vector<size_t> out;

	for(const QString & v : _selected)
		out.push_back(mapValueToRow[v]);

	std::sort(out.begin(), out.end());

	return out;
}

void ColumnModel::setValueMaxWidth()
{
	size_t maxWidthChars = std::max(size_t(tr("Value").size()), !column() ? 0 : column()->getMaximumWidthInCharacters(false, true));
	
	double prevMaxWidth = _valueMaxWidth;
	_valueMaxWidth = JaspTheme::fontMetrics().size(Qt::TextSingleLine, QString(maxWidthChars, 'X')).width();

	if(_valueMaxWidth != prevMaxWidth)
		emit valueMaxWidthChanged();
}

void ColumnModel::setLabelMaxWidth()
{
	size_t maxWidthChars = std::max(size_t(tr("Label").size()), !column() ? 0 : column()->getMaximumWidthInCharacters(false, false));
	
	double prevMaxWidth = _labelMaxWidth;
	_labelMaxWidth = JaspTheme::fontMetrics().size(Qt::TextSingleLine, QString(maxWidthChars, 'X')).width();

	if(_labelMaxWidth != prevMaxWidth)
		emit labelMaxWidthChanged();
}

void ColumnModel::moveSelectionUp()
{
	std::vector<size_t> indexes = getSortedSelection();
	if (_beingRefreshed || indexes.size() < 1)
		return;

	_lastSelected = -1;
	// The excision, Cut 4: label reordering is legacy (the label editor returns in B2 on
	// the jasp:labels overlay — value-keyed, no per-row state).
	Log::log() << "ColumnModel::moveSelectionUp: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::moveSelectionDown()
{
	std::vector<size_t> indexes = getSortedSelection();
	if (_beingRefreshed || indexes.size() < 1)
		return;

	_lastSelected = -1;
	Log::log() << "ColumnModel::moveSelectionDown: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::reverse()
{
	if (_beingRefreshed)
		return;

	_lastSelected = -1;
	Log::log() << "ColumnModel::reverse: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::reverseValues()
{
	if (_beingRefreshed)
		return;

	_lastSelected = -1;
	Log::log() << "ColumnModel::reverseValues: legacy label/value ops return with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::toggleAutoSortByValues()
{
	_lastSelected = -1;
	Log::log() << "ColumnModel::toggleAutoSortByValues: legacy label/value ops return with the labels editor (B2) — ignored" << std::endl;
}

bool ColumnModel::setData(const QModelIndex & index, const QVariant & value, int role)
{
	if(role == int(dataPkgRoles::selected))
		return false;

	bool result = QIdentityProxyModel::setData(index, value, role);

	if (!_editing && (role == Qt::EditRole || role == int(dataPkgRoles::filter)))
		setSelected(index.row(), 0);

	return result;
}

QVariant ColumnModel::data(	const QModelIndex & index, int role) const
{
	if(role == int(dataPkgRoles::selected))
	{
		bool s = _selected.count(data(index, int(dataPkgRoles::value)).toString()) > 0;
		return s;
	}

	return QIdentityProxyModel::data(index, role > 0 ? role : int(dataPkgRoles::label));
}

QVariant ColumnModel::headerData(int section, Qt::Orientation orientation, int role) const
{
	if(role == int(dataPkgRoles::columnWidthFallback))
		return rowWidth();
	
	return !sourceModel() ? false : sourceModel()->headerData(section, orientation, role);
}

void ColumnModel::refreshFilteredOut()
{
	JASPTIMER_SCOPE(ColumnModel::refreshFilteredOut);

	//Re-query the chosen column's label-filter state so QML's `filteredOut` reacts to label-filter
	//changes while the chosen column itself stays unchanged (previously only re-emitted on selection).
	emit filteredOutChanged();
	emit columnIsFilteredChanged();
}

int ColumnModel::filteredOut() const
{
	return !column() ? 0 :column()->filteredOut();
}

void ColumnModel::resetFilterAllows()
{
	column()->resetFilterAllows();
}

void ColumnModel::setVisible(bool visible)
{
	//visible = visible && rowCount() > 0; //cannot show labels when there are no labels

	if (_visible == visible)
		return;

	_visible = visible;
	emit visibleChanged(_visible);
}

Column * ColumnModel::column() const
{
	return _column;
}

int ColumnModel::chosenColumn() const
{
	// NEO (R2 step 4): no mirror Column exists on lane data — the maintained index is the
	// truth (the variables list binds this for its currentIndex).
	DataSet * laneDataSet = _shownDataSet ? _shownDataSet : DataSetPackage::pkg()->dataSet();
	if (laneDataSet && laneDataSet->isOpen())
		return _virtual ? -1 : _columnIndex;

	Column * c = column();
	
	if(!c)
		return -1;
	
	if(!c->data())
		return -1;
	
	return c->data()->columnIndex(c);
}

void ColumnModel::setChosenColumnByName(const QString chosenNameQ, int colIndex)
{
	std::string chosenName = fq(chosenNameQ);
	// Always set the chosen column even if it is the same one: the ColumnModel might be not reset correctly when the dataset is closed.

	//If the user deletes the name the column ought to be removed because we cannot have columns without a name!
	Column * deleteMe = column() && column()->name() == "" ? column() : nullptr;

	emit beforeChangingColumn(chosenNameQ);

	DataSet * data = DataSetPackage::pkg()->dataSet();
	clearVirtual();
	
	// NEO (R2 step 4): resolve by the SCHEMA name; NO mirror Column exists — the schema
	// index becomes the chosen index, and the legacy bindings (labels/computed) stay
	// inert-by-null on lane data (both are gated future eras).
	int laneSchemaIdx = -1;
	Column * chosenColumn = nullptr;
	if (data && data->isOpen())
		laneSchemaIdx = data->schemaColumnIndex(chosenName);
	else
		chosenColumn = data ? data->column(chosenName) : nullptr;

	//Drop any per-column connections to the *previous* column before switching to the new one,
	//otherwise the old column (still alive in the dataset) keeps firing into this model.
	if(_column && _column != chosenColumn)
		disconnect(_column, nullptr, this, nullptr);
	
	// NEO (R2 step 4): no mirror Column exists on lane — virtual means NOT FOUND IN THE
	// SCHEMA, not "no Column pointer" (leaving it `!chosenColumn` marked every clicked
	// lane column virtual: the editor showed an empty "new column" form and typing a name
	// INSERTED one instead of renaming).
	_virtual = !chosenColumn && laneSchemaIdx < 0;
	emit isVirtualChanged();

	setSourceModel(chosenColumn);
	_column = chosenColumn;
	
	
	if(_column)
	{
		connect(_column,	&Column::modelReset,					this, &ColumnModel::rowsTotalChanged,			Qt::UniqueConnection);
		connect(_column,	&Column::columnChanged,					this, &ColumnModel::setLabelMaxWidth,			Qt::UniqueConnection);
	}

	
	_columnIndex = colIndex != -1 || _virtual || !chosenColumn || !chosenColumn->data() ? colIndex : chosenColumn->data()->columnIndex(chosenColumn);
	if (laneSchemaIdx >= 0)
		_columnIndex = laneSchemaIdx;	// NEO: the schema index IS the chosen index (no mirror to ask)

	refresh();
	notifyColumnChanged();

	if(deleteMe && data)
	{
		const int doomedIdx = data->columnIndex(deleteMe);
		if(doomedIdx >= 0)
			data->removeColumn(doomedIdx);
	}
}

void ColumnModel::setChosenColumn(int columnIndex)
{
	// NEO (R2 step 4): resolve the index against the SCHEMA directly. The legacy responder
	// (DataSetTableModel::columnName — MainWindow's columnNameForIndex connection) reads
	// the legacy model, EMPTY on lane datasets, and the "" fallthrough below opened the
	// virtual "new column" form for every click: empty fields, and typing a name then
	// INSERTED a column instead of editing the clicked one.
	DataSet * laneDataSet = _shownDataSet ? _shownDataSet : DataSetPackage::pkg()->dataSet();
	if (laneDataSet && laneDataSet->isOpen())
	{
		const ColumnInfo * info = columnIndex >= 0 ? laneDataSet->schemaColumnAt(size_t(columnIndex)) : nullptr;
		if (info)
		{
			setChosenColumnByName(tq(info->name));
			return;
		}
		// an index at/past the schema extent IS the virtual new-column slot — fall through
	}

	QString name = emit columnNameForIndex(columnIndex);
	
	if(name != "")
	{
		setChosenColumnByName(name);
		return;
	}
	
	_columnIndex = columnIndex;
	
	_virtual = true;
	emit isVirtualChanged();
	
	setSourceModel(nullptr);
	
	_column = nullptr;
	refresh();
	notifyColumnChanged();
	
}

void ColumnModel::checkInsertedColumns(const QModelIndex &, int first, int)
{
	if (_columnIndex >= first)
	{
		_columnIndex = -1; // Force the setting of new column.
		setChosenColumn(first);
	}
}

void ColumnModel::checkRemovedColumns(int columnIndex, int count)
{
	int currentCol = chosenColumn();
	if ((columnIndex <= currentCol) && (currentCol < columnIndex + count))
	{
		setVisible(false);
		setChosenColumn(-1);
	}
}

void ColumnModel::openComputedColumn(const QString name)
{
	setChosenColumnByName(name);
	setVisible(true);
}

void ColumnModel::checkCurrentColumn(int dataSetId, QStringList, QStringList missingColumns, QMap<QString, QString> changeNameColumns, bool, bool hasNewColumns)
{
	DataSet * current = DataSetPackage::pkg()->dataSet();
	if(!current || dataSetId != current->id())
		return;
	
	QString colName = columnNameQ();

	if (missingColumns.contains(colName))
	{
		setVisible(false);
		setChosenColumn(-1);
	}
	else
	{
		if (!_virtual && changeNameColumns.contains(colName))
			setColumnNameQ(changeNameColumns[colName]);
		if (hasNewColumns && _virtual && DataSetPackage::pkg()->dataSet()->columnCount() >= _columnIndex)
		{
			// The current column is not virtual anymore: reset it
			_columnIndex = -1;
			setChosenColumnByName(colName);
		}
	}
}

void ColumnModel::shownDataSetChangedHandler(DataSet * newDataSet)
{
	_shownDataSet = newDataSet;

	if(!newDataSet)
	{
		//Teardown (deleteWorkspace/connectWorkspace between datasets) left _column pointing at a
		//just-destroyed dataset's column: clear it so the Variables/label editor doesn't dereference
		//freed memory, and drop any per-column connections to the old column.
		if(_column)
		{
			disconnect(_column, nullptr, this, nullptr);
			setSourceModel(nullptr);
			_column = nullptr;
			_virtual = true;
			emit isVirtualChanged();
			emit filteredOutChanged();
			emit columnIsFilteredChanged();
		}
		return;
	}

	//Label-level filtering toggles don't fire datasetChanged, so hook the shown dataset's dedicated
	//signal to keep `filteredOut`/`columnIsFiltered` reactive while the chosen column is unchanged.
	connect(newDataSet, &DataSet::labelFilterChanged, this, &ColumnModel::refreshFilteredOut, Qt::UniqueConnection);

	// NEO adapter: a lane edit lands as schemaChanged (applyRevision → landWireSchema) —
	// nothing bridges it to this model's legacy refresh chain (datasetChanged), so without
	// this hook the editor's fields would keep serving the pre-edit state forever.
	connect(newDataSet, &DataSet::schemaChanged, this, &ColumnModel::laneSchemaRefreshed, Qt::UniqueConnection);

	if(!column())
		return;

	QString currentName = columnNameQ();
	if(currentName.isEmpty())
		return;

	//Another dataset became the shown one: re-resolve the currently chosen column (by name)
	//against the new shown dataset so the Variables/label editor follows the tab switch instead of
	//silently editing a non-shown dataset's column.
	setChosenColumnByName(currentName);
}

const ColumnInfo * ColumnModel::laneSchemaColumn() const
{
	DataSet * dataSet = _shownDataSet ? _shownDataSet : DataSetPackage::pkg()->dataSet();
	if (!dataSet || !dataSet->isOpen())
		return nullptr;

	// The chosen column by SCHEMA index: _columnIndex is maintained by the choose paths
	// (setChosenColumn stores the view's index — schema order; setChosenColumnByName's
	// NEO branch stores schemaColumnIndex) — no mirror Column exists to ask (R2 step 4).
	return !_virtual && _columnIndex >= 0 ? dataSet->schemaColumnAt(size_t(_columnIndex)) : nullptr;
}

void ColumnModel::laneSchemaRefreshed()
{
	// Only the SHOWN dataset's schema matters (a background dataset landing must not
	// disturb the editor) — and only lane datasets fire this usefully.
	DataSet * dataSet = _shownDataSet ? _shownDataSet : DataSetPackage::pkg()->dataSet();
	if (dataSet && sender() == dataSet && dataSet->isOpen())
	{
		refresh();
		notifyColumnChanged();	// re-fires chosenColumnChanged/columnTitleChanged/… — the QML re-reads the schema
	}
}

void ColumnModel::removeAllSelected()
{
	QMap<QString, size_t> mapValueToRow;

	for(size_t r=0; r<size_t(rowCount()); r++)
		mapValueToRow[data(index(r, 0), int(dataPkgRoles::value)).toString()] = r;

	QVector<QString> selectedValues;
	for (const QString& s : _selected)
		selectedValues.append(s);

	_selected.clear();
	_lastSelected = -1;
	for (const QString& selectedValue : selectedValues)
	{
		if (mapValueToRow.contains(selectedValue))
		{
			int selectedRow = int(mapValueToRow[selectedValue]);
            emit dataChanged(ColumnModel::index(selectedRow, 0), ColumnModel::index(selectedRow, 0), {int(dataPkgRoles::selected)});
		}
	}
}

void ColumnModel::setRowWidth(double len)
{
	if(std::abs(_rowWidth - len) < 0.001)
		return;
	
	_rowWidth = len;
	emit rowWidthChanged();
	refresh();
}

void ColumnModel::refresh()
{
	beginResetModel();
	endResetModel();
}

void ColumnModel::notifyColumnChanged()
{
	setValueMaxWidth();
	setLabelMaxWidth();

	emit chosenColumnChanged();
	emit filteredOutChanged();
	emit nameEditableChanged();
	emit computedTypeChanged();
	emit computedTypeEditableChanged();
	emit computedTypeValuesChanged();
	emit columnTypeChanged();
	emit columnTypeValuesChanged();
	emit hasSeveralNumericValuesChanged();
	emit rowsTotalChanged();
	emit tabsChanged();
	emit useCustomEmptyValuesChanged();
	emit emptyValuesChanged();
	emit dropLevelsChanged();
	emit columnIsFilteredChanged();
	// The editor-field properties (the R2 adapter rebinds ColumnBasicInfo's TextFields to
	// these): without these emits the Long-name/Description/Use-labels fields NEVER rebind
	// on a column switch — they kept serving the PREVIOUS column's values (the "infection"
	// seen as values that "keep and get set"). Name heals via chosenColumnChanged; these
	// three were simply missing from the notify set.
	emit columnTitleChanged();
	emit columnDescriptionChanged();
	emit hasLabelsChanged();
}

void ColumnModel::setSelected(int row, int modifier)
{
	if (modifier & Qt::ShiftModifier && _lastSelected >= 0)
	{
		int start = _lastSelected >= row ? row : _lastSelected;
		int end = start == _lastSelected ? row : _lastSelected;
		for (int i = start; i <= end; i++)
		{
			QString rowValue = data(index(i, 0), int(dataPkgRoles::value)).toString();
			_selected.insert(rowValue);
            emit dataChanged(ColumnModel::index(i, 0), ColumnModel::index(i, 0), {int(dataPkgRoles::selected)});
		}
	}
	else if (modifier & Qt::ControlModifier)
	{
		QString rowValue = data(index(row, 0), int(dataPkgRoles::value)).toString();
		_selected.insert(rowValue);
        emit dataChanged(ColumnModel::index(row, 0), ColumnModel::index(row, 0), {int(dataPkgRoles::selected)});
	}
	else
	{
		QString rowValue = data(index(row, 0), int(dataPkgRoles::value)).toString();
		bool disableCurrent = _selected.count(rowValue) > 0;
		removeAllSelected();
		
		if (!disableCurrent)	_selected.insert(rowValue);
		else					_selected.erase(rowValue);
        emit dataChanged(ColumnModel::index(row, 0), ColumnModel::index(row, 0), {int(dataPkgRoles::selected)});
	}
	
	_lastSelected = row;

}

void ColumnModel::unselectAll()
{
	_selected.clear();
	_lastSelected = -1;
	refresh(); //emit dataChanged(ColumnModel::index(0, 0), ColumnModel::index(rowCount(), 0), {int(dataPkgRoles::selected)});
}

bool ColumnModel::setChecked(int rowIndex, bool checked)
{
	JASPTIMER_SCOPE(ColumnModel::setChecked);
	
	if(_beingRefreshed || checked == data(index(rowIndex,0), int(dataPkgRoles::filter)).toBool())
		return true; //Its already that value
	
	setSelected(rowIndex, true);

	// The excision, Cut 4: label-level filtering is legacy (filters return as derived
	// boolean columns); the label editor returns in B2.
	Log::log() << "ColumnModel::setChecked: label filtering is legacy — ignored" << std::endl;
	
	return data(index(rowIndex, 0), int(dataPkgRoles::filter)).toBool() == checked;
}

void ColumnModel::setValue(int rowIndex, const QString &value)
{
	JASPTIMER_SCOPE(ColumnModel::setValue);
	
	QString originalValue = data(index(rowIndex,0), int(dataPkgRoles::value)).toString();
	
	if(_beingRefreshed || value == originalValue)
		return; //Its already that value
	
	Log::log() << "ColumnModel::setValue: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::setLabel(int rowIndex, QString label)
{
	JASPTIMER_SCOPE(ColumnModel::setLabel);
	
	QString originalLabel = data(index(rowIndex,0), int(dataPkgRoles::label)).toString();
	
	if(_beingRefreshed || label == originalLabel)
		return; //Its already that value
	
	Log::log() << "ColumnModel::setLabel: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::deleteLabel(int rowIndex)
{
	Q_UNUSED(rowIndex);
	Log::log() << "ColumnModel::deleteLabel: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

void ColumnModel::addLabel(QString value, QString label)
{
	Q_UNUSED(value);
	Q_UNUSED(label);
	Log::log() << "ColumnModel::addLabel: label editing returns with the labels editor (B2) — ignored" << std::endl;
}

bool ColumnModel::columnIsFiltered() const
{
	return column() && column()->hasLabelFilter();
}

bool ColumnModel::nameEditable() const
{
	if(column())
		return !(column()->isComputed() && (column()->codeType() == computedColumnType::analysisNotComputed || column()->codeType() == computedColumnType::analysis));

	return true;
}

void ColumnModel::clearVirtual()
{
	_dummyColumn.description.clear();
	_dummyColumn.name.clear();
	_dummyColumn.title.clear();
	_dummyColumn.computeFilter.clear();

	_dummyColumn.type			= columnType::scale;
	_dummyColumn.computedType	= computedColumnType::notComputed;
}

bool ColumnModel::compactMode() const
{
	return _compactMode;
}

void ColumnModel::setCompactMode(bool newCompactMode)
{
	if (_compactMode == newCompactMode)
		return;
	_compactMode = newCompactMode;
	emit compactModeChanged();
	emit tabsChanged();
}

void ColumnModel::languageChangedHandler()
{
	emit columnTypeValuesChanged();
	emit computedTypeValuesChanged();
	emit tabsChanged();
}

bool ColumnModel::hasLabels() const
{
	// NEO adapter: no labels on lane data until B2 (the jasp:labels overlay era) — and
	// notably the mirror's flag is meaningless there anyway (it is a STORAGE mode).
	if (laneSchemaColumn())
		return false;

	return column() ? column()->hasLabels() : false;
}

void ColumnModel::setHasLabels(bool newHasLabels)
{
	if (_beingRefreshed)
		return;

	// NEO gate: hasLabels is legacy's STORAGE MODE; the NEO equivalent is the jasp:labels
	// overlay (P11), which arrives with the labels editor (B2). When built, this control
	// enables off ColumnInfo::distinctCount vs WIRE_LEVELS_CAP — one source of truth.
	DataSet * neoDataSet = DataSetPackage::pkg()->dataSet();
	if (neoDataSet && neoDataSet->isOpen())
	{
		Log::log() << "ColumnModel::setHasLabels: labels arrive with the labels editor (B2) on lane datasets — ignored" << std::endl;
		return;
	}

	if(column())
	{
		// The excision, Cut 4: hasLabels was legacy's STORAGE MODE; the NEO equivalent is
		// the jasp:labels overlay (P11, B2). The branch is unreachable on lane (column()
		// is null there) and inert without its command.
	}
}

bool ColumnModel::isColumnNameFree(const QString & name)
{
	DataSet * dataSet = DataSetPackage::pkg()->dataSet();

	// NEO: the lane schema is the name space (GridModel's twin) — the mirror's names go
	// stale after NEO renames (grow-only, never renamed).
	if (dataSet && dataSet->isOpen())
		return dataSet->schemaColumnIndex(fq(name)) < 0;

	return dataSet && !dataSet->column(fq(name));
}

void ColumnModel::createComputedColumn(const QString & name, int colType, bool useJsonConstructor)
{
	DataSet * dataSet = DataSetPackage::pkg()->dataSet();

	if(!dataSet || !isColumnNameFree(name))
		return;

	// NEO gate: computed columns on lane data are a future era (analyses).
	if (dataSet->isOpen())
	{
		Log::log() << "ColumnModel::createComputedColumn: computed columns are not yet available on lane datasets — ignored" << std::endl;
		return;
	}

	Column * column = Workspace::singleton()->createComputedColumn(
		fq(name),
		dataSet->id(),
		-1,
		columnType(colType),
		useJsonConstructor ? computedColumnType::constructorCode : computedColumnType::rCode);

	if(column)
		openComputedColumn(name);
}
