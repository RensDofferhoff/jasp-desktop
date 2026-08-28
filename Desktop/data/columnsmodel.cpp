#include "log.h"
#include "qutils.h"
#include "jasptheme.h"
#include "dataenums.h"
#include "mainwindow.h"
#include "columnsmodel.h"
#include "dataset.h"
#include "workspace.h"

ColumnsModel * ColumnsModel::_singleton = nullptr;

ColumnsModel::ColumnsModel(DataSetTableModel *tableModel) 
: QAbstractTableModel(tableModel), _tableModel(tableModel)
{
	assert(!_singleton);
	_singleton = this;
	
	connect(_tableModel, &DataSetTableModel::columnTypeChanged,		this, &ColumnsModel::columnTypeChanged	);
	connect(_tableModel, &DataSetTableModel::labelChanged,			this, [&](const Column * col, QString orgLabel, QString newLabel) { emit labelsChanged(col->nameQ(), QMap<QString, QString>{std::make_pair(orgLabel, newLabel) }); } );
	connect(_tableModel, &DataSetTableModel::labelsReordered,		this, &ColumnsModel::labelsReordered	);
	connect(_tableModel, &DataSetTableModel::emptyValuesChanged,	this, &ColumnsModel::dataSetChanged		);
	connect(_tableModel, &DataSetTableModel::modelReset,			this, &ColumnsModel::refresh			);
	connect(_tableModel, &DataSetTableModel::dataChanged,			this, &ColumnsModel::refresh			);
	
	auto * info = new VariableInfo(_singleton);

	connect(this, &ColumnsModel::columnNamesChanged,					info, &VariableInfo::variableNamesChanged	);
	connect(this, &ColumnsModel::columnsChanged,						info, &VariableInfo::variablesChanged		);

	connect(this,						&ColumnsModel::labelsChanged,				info, &VariableInfo::labelsChanged			);
	connect(this,						&ColumnsModel::labelsReordered,				info, &VariableInfo::labelsReordered		);
	connect(this,						&ColumnsModel::filterChanged,				info, &VariableInfo::filterChanged			);
	connect(this,						&ColumnsModel::dataSetChanged,				info, &VariableInfo::dataSetChanged			);
	connect(this,						&QAbstractTableModel::modelReset,			info, &VariableInfo::rowCountChanged		);
	connect(_tableModel,				&DataSetTableModel::columnsInserted,		info, &VariableInfo::rowCountChanged		);
	connect(_tableModel,				&DataSetTableModel::columnsRemoved,			info, &VariableInfo::rowCountChanged		);
	connect(MainWindow::singleton(),	&MainWindow::dataAvailableChanged,			info, &VariableInfo::dataAvailableChanged	);

	// Wide-data fix (2026-08-16): the cached dataset Terms (dataSetTerms()) must be rebuilt
	// whenever the column set can have changed. Redundant invalidation is cheap (one lazy
	// rebuild); a missed one would show stale variables, so err on the generous side.
	connect(_tableModel, &DataSetTableModel::columnTypeChanged,	this, [this]() { _dataSetTermsValid = false; });
	connect(_tableModel, &DataSetTableModel::modelReset,		this, [this]() { _dataSetTermsValid = false; });
	connect(_tableModel, &DataSetTableModel::dataChanged,		this, [this]() { _dataSetTermsValid = false; });
	connect(_tableModel, &DataSetTableModel::columnsInserted,	this, [this]() { _dataSetTermsValid = false; });
	connect(_tableModel, &DataSetTableModel::columnsRemoved,	this, [this]() { _dataSetTermsValid = false; });
	connect(_tableModel, &DataSetTableModel::emptyValuesChanged,this, [this]() { _dataSetTermsValid = false; });
	connect(this, &ColumnsModel::columnNamesChanged,			this, [this](QMap<QString, QString>) { _dataSetTermsValid = false; });
	connect(this, &ColumnsModel::dataSetChanged,				this, [this]() { _dataSetTermsValid = false; });

	// Multi-dataset fold (data-model-design.md §3.4): serve the SHOWN dataset; whenever it is
	// orchestrator-backed the wire schema is the source of truth, else the legacy table serves.
	if (DataSetPackage::pkg() && DataSetPackage::pkg()->workspace())
	{
		connect(DataSetPackage::pkg()->workspace(), &Workspace::shownDataSetChanged, this,
				[this](DataSet *) { bindLane(DataSetPackage::pkg()->workspace()->shownDataSet()); });
		bindLane(DataSetPackage::pkg()->workspace()->shownDataSet());
	}
}

ColumnsModel::~ColumnsModel()
{ 
	if(_singleton == this) 
		_singleton = nullptr;
}

void ColumnsModel::bindLane(DataSet * dataSet)
{
	if (_laneDataSet == dataSet)
		return;

	_dataSetTermsValid = false;	// dataset switch: cached Terms are stale

	beginResetModel();

	if (_laneDataSet)
		disconnect(_laneDataSet, nullptr, this, nullptr);

	_laneDataSet = dataSet;

	if (_laneDataSet)
		connect(_laneDataSet, &DataSet::schemaChanged, this, [this]()
		{
			beginResetModel();
			endResetModel();
			emit dataSetChanged();
		});

	endResetModel();	// the ctor connects modelReset -> VariableInfo::rowCountChanged

	emit dataSetChanged();
}

QString ColumnsModel::getColumnIcon(int colType) const
{
	return getColumnIcon(columnType(colType));
}

QString ColumnsModel::getColumnIcon(int colType, bool isTransformed) const
{
	return !isTransformed ? getColumnIcon(colType) : getColumnIconTransform(colType);
}

QString ColumnsModel::getColumnIcon(columnType colType) const
{
	return JaspTheme::currentIconPath() + "/"+ getIconFilename(colType, varIconType::DefaultIconType);
}

QString ColumnsModel::getColumnDescription(const QString &name) const
{
	return provideInfo(varInfoType::ColumnDescription, name).toString().trimmed();
}

QString ColumnsModel::getColumnIconTransform(int colType) const
{
	return getColumnIconTransform(columnType(colType));
}

QString ColumnsModel::getColumnIconTransform(columnType colType) const
{
	return JaspTheme::currentIconPath() + "/"+ getIconFilename(colType, varIconType::TransformedIconType);
}

int ColumnsModel::getColumnType(const QString & columnName) const
{
	int index = ColumnsModel::singleton()->getColumnIndex(fq(columnName));
	
	if(index == -1)
		return int(columnType::unknown);
	
	return ColumnsModel::singleton()->data(ColumnsModel::singleton()->index(index, 0), ColumnTypeRole).toInt();
}

QString ColumnsModel::getColumnTransformedToolTip(const QString &name, int transformedTo) const
{
	return 	getColumnTransformedToolTip(name, columnType(transformedTo));
}

QString ColumnsModel::getColumnTransformedToolTip(const QString &name, columnType chosenType) const
{
	columnType	realType	= columnType(getColumnType(name));
	
	if(ColumnsModel::singleton()->getColumnIndex(fq(name)) == -1 || chosenType == realType)
		return "";
	
	varInfoType		previewType;
	
	switch(chosenType)
	{
	default:					previewType = varInfoType::PreviewScale;		break;
	case columnType::ordinal:	previewType	= varInfoType::PreviewOrdinal;		break;
	case columnType::nominal:	previewType	= varInfoType::PreviewNominal;		break;
	}
	
	return provideInfo(previewType, name).toString();
	
}


QVariant ColumnsModel::data(const QModelIndex &index, int role) const
{
	QString				colName;
	columnType			colType;
	computedColumnType	codeType;

	if (_laneDataSet && _laneDataSet->isOpen())
	{
		const ColumnInfo * col = _laneDataSet->schemaColumnAt(size_t(index.row()));
		if (!col)
			return QVariant();

		colName		= tq(col->name);
		colType		= col->type;
		codeType	= col->codeType;
	}
	else
	{
		colName		=										 _tableModel->headerData(index.row(), Qt::Horizontal, int(dataPkgRoles::name				)).toString();
		colType		= static_cast<columnType>			(_tableModel->headerData(index.row(), Qt::Horizontal, int(dataPkgRoles::columnType			)).toInt());
		codeType	= static_cast<computedColumnType>	(_tableModel->headerData(index.row(), Qt::Horizontal, int(dataPkgRoles::computedColumnType	)).toInt());
	}

	switch(role)
	{
	case NameRole:					return colName;
	case TypeRole:					return "column";
	case ColumnTypeRole:			return int(colType);
	case ComputedColumnTypeRole:	return int(codeType);
	case IconSourceRole:			return JaspTheme::currentIconPath() + "/"+ getIconFilename(colType, varIconType::DefaultIconType);
	case ToolTipRole:
	{
		QString		usedIn	= colType == columnType::scale		? tr("which can be used in numerical comparisons and mathematical operations.")
							: colType == columnType::ordinal	? tr("which can only be used in (in)equivalence, greater and lesser than comparisons. Not in mathematical operations as subtraction etc, to do so: try converting to scalar first.")
																: tr("which can only be used in (in)equivalence comparisons. Not in greater/lesser-than comparisons or mathematical operations, to do so: try converting to ordinal or scalar first.");

		return tr("The '") + colName + tr("'-column ") + usedIn;
	}
	}
	
	return _tableModel->data(_tableModel->index(index.column(), index.row()), role);
}

int ColumnsModel::rowCount(const QModelIndex &) const
{
	return _laneDataSet && _laneDataSet->isOpen() ? int(_laneDataSet->schema().size()) : _tableModel->columnCount();
}

int ColumnsModel::columnCount(const QModelIndex &) const
{
	return 1;
}

QVariant ColumnsModel::provideInfo(varInfoType info, const QString& colName, int row) const
{
	ColumnsModel* colModel = ColumnsModel::singleton();

	if (!colModel)
		return QVariant();

	try
	{
		// Wide-data fast path (2026-08-16): the full (name, type) Terms of the active dataset,
		// cached — one request + one copy instead of k per-name roundtrips per reset pass.
		if (info == varInfoType::DataSetTerms)
			return QVariant::fromValue(colModel->dataSetTerms());

		int colIndex = colName.isEmpty() ? 0 : colModel->getColumnIndex(fq(colName));

		if (colIndex < 0)
			return QVariant();

		// NEO lane dataset (data-model-design.md §3.4): schema info comes from the shown DataSet's wire schema.
		// Value-flavoured info has no frontend source until data_view lands — return empty.
		if (colModel->_laneDataSet && colModel->_laneDataSet->isOpen())
		{
			const ColumnInfo	* col = colModel->_laneDataSet->schemaColumnAt(size_t(colIndex));

			if (!col)
				return QVariant();

			switch(info)
			{
			case varInfoType::VariableType:		return int(col->type);
			case varInfoType::NameRole:			return ColumnsModel::NameRole;
			case varInfoType::VariableNames:		return getColumnNames();
			case varInfoType::DataAvailable:		return MainWindow::singleton()->dataAvailable();
			case varInfoType::DataSetRowCount:	return qulonglong(colModel->_laneDataSet->schemaRows());
			case varInfoType::Labels:
			{
				QStringList levels;
				for (const std::string & level : col->levels)
					levels.append(tq(level));
				return levels;
			}
			case varInfoType::TotalLevels:			return qulonglong(col->distinctCount);	// distinct_count is the single source of truth for counts — wire levels are a capped UI prefix (design doc §2, decision 15)
			case varInfoType::TotalNumericValues:
				if (col->type == columnType::scale)
					return qulonglong(col->distinctCount);	// scale: every value is numeric — the same number legacy's O(N log N) nonFilteredNumericsCount scan produced, without any scan
				// Categoricals (e.g. jaspReliability SEM gates minNumericLevels:2 on nominal/
				// ordinal items): the lane computes the distinct NUMERIC level count locale-aware
				// (numeric_levels) — the frontend never parses wire values (design doc §2).
				return col->numericLevels;
			case varInfoType::ColumnDescription:	return tq(col->description);
			case varInfoType::DataSetPointer:		return QVariant::fromValue<void*>(nullptr);	// deliberately no raw handout (design decision 8)
			default:								return QVariant();	// values/previews: nothing in the frontend until data_view
			}
		}

		QModelIndex qColIndex	= index(colIndex, 0),
					tableCIndex	= _tableModel->index(0, colIndex),
					tableVIndex	= _tableModel->index(row, colIndex);

		//columnType	colTypeHere	= static_cast<columnType>(colTypeInt);

		switch(info)
		{
		case varInfoType::VariableType:				return					data(qColIndex, ColumnsModel::ColumnTypeRole).toInt();
		case varInfoType::NameRole:					return					data(qColIndex, ColumnsModel::NameRole);
		
		case varInfoType::DoubleValues:				return	_tableModel->	data(tableCIndex,						int(dataPkgRoles::valuesDblList));
		case varInfoType::TotalNumericValues:		return	_tableModel->	data(tableCIndex,						int(dataPkgRoles::nonFilteredNumericValuesCount));
		case varInfoType::TotalLevels:				return	_tableModel->	data(tableCIndex,						int(dataPkgRoles::nonFilteredLevels)).toStringList().length();
		case varInfoType::Labels:					return	_tableModel->	data(tableCIndex,						int(dataPkgRoles::nonFilteredLevels));
		case varInfoType::DataSetValues:			return	_tableModel->	data(tableCIndex,						int(dataPkgRoles::valuesStrList));
		case varInfoType::DataSetRowCount:			return  _tableModel->	rowCount();
		case varInfoType::SignalsBlocked:			return	_tableModel->	synchingData();
		case varInfoType::DataSetValue:				return	_tableModel->	data(tableVIndex,						int(dataPkgRoles::value));
		
		case varInfoType::VariableNames:			return	getColumnNames();
		case varInfoType::DataAvailable:			return	MainWindow::singleton()->dataAvailable();
		
		case varInfoType::MaxWidth:					return	_tableModel->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::maxColString)).toInt();
		case varInfoType::PreviewScale:				return	_tableModel->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::previewScale));
		case varInfoType::PreviewOrdinal:			return	_tableModel->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::previewOrdinal));
		case varInfoType::PreviewNominal:			return	_tableModel->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::previewNominal));
		case varInfoType::ColumnDescription:		return	_tableModel->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::description));
		case varInfoType::DataSetPointer:			return	QVariant::fromValue<void*>(DataSetPackage::pkg()->dataSet());
		}
	}
	catch(std::exception & e)
	{
		Log::log() << "AnalysisForm::requestInfo had an exception! " << e.what() << std::flush;
		throw e;
	}

	return QVariant();
}

bool ColumnsModel::absorbInfo(varInfoType info, const QString &colName, int row, QVariant value)
{
	ColumnsModel* colModel = ColumnsModel::singleton();

	if (!colModel)
		return false;

	if (colModel->_laneDataSet && colModel->_laneDataSet->isOpen())
		return false;	// NEO: no frontend cell writes until data_edit lands (data-model-design.md §3.4)

	try
	{
		int colIndex = colModel->getColumnIndex(fq(colName));
		if (colIndex < 0)
			return false;

		QModelIndex qColIndex	= _tableModel->index(0, colIndex),
					qValIndex	= _tableModel->index(row, colIndex);

		switch(info)
		{
		default:										return	false;
		case varInfoType::DataSetValue:					return	_tableModel->setData(qValIndex, value,	int(dataPkgRoles::value));
		case varInfoType::DataSetValues:				return	_tableModel->setData(qColIndex, value,	int(dataPkgRoles::valuesStrList));
		}
	}
	catch(std::exception & e)
	{
		Log::log() << "AnalysisForm::requestInfo had an exception! " << e.what() << std::flush;
		throw e;
	}

	return false;
}

QHash<int, QByteArray> ColumnsModel::roleNames() const
{
	//These should be the same as used in ElementView.qml
	static const auto roles = QHash<int, QByteArray>{
		{ NameRole,					"columnName"			},
		{ TypeRole,					"type"					},
		{ ColumnTypeRole,			"columnType"			},
		{ ComputedColumnTypeRole,	"computedColumnType"	},
		{ IconSourceRole,			"columnIcon"			},
		{ ToolTipRole,				"toolTip"				}
	};

	return roles;
}

QStringList ColumnsModel::getColumnNames() const
{
	QStringList result;

	int rows = rowCount();
	for (int i = 0; i < rows; i++)
		result.append(data(index(i, 0), NameRole).toString());

	return result;
}

const Terms & ColumnsModel::dataSetTerms() const
{
	if (_dataSetTermsValid)
		return _dataSetTermsCache;

	_dataSetTermsCache.clear();

	if (_laneDataSet && _laneDataSet->isOpen())
	{
		const size_t count = _laneDataSet->schema().size();
		for (size_t i = 0; i < count; i++)
			if (const ColumnInfo * col = _laneDataSet->schemaColumnAt(i))
				_dataSetTermsCache.add(Term(tq(col->name), col->type));
	}
	else
	{
		// Legacy table: same walk the old per-name loop did, centralized and rebuilt only on change.
		const QStringList names = getColumnNames();
		for (const QString & name : names)
			_dataSetTermsCache.add(Term(name, columnType(getColumnType(name))));
	}

	_dataSetTermsValid = true;
	return _dataSetTermsCache;
}

void ColumnsModel::datasetChanged(  int																						dataSetID,
												QStringList                             changedColumns,
									QStringList                             missingColumns,
									QMap<QString, QString>					changeNameColumns,
									bool                                    rowCountChanged,
									bool                                    hasNewColumns)
{
	//Only the shown dataset drives the visible column list (and the VariableInfo provider bound to it);
	//ignore column changes coming from background datasets.
	DataSet * shown = DataSetPackage::pkg()->dataSet();
	if(!shown || dataSetID != shown->id())
		return;

	   if(! (missingColumns.size() > 0 || hasNewColumns))
	   {
			   if (changeNameColumns.size() > 0)
					   emit columnNamesChanged(changeNameColumns);
			   else if (changedColumns.size() > 0 || rowCountChanged)
			   {
					   if (rowCountChanged)
					   {
							   changedColumns.clear();
							   for (int i = 0; i < rowCount(); i++)
									   changedColumns.push_back(data(index(i, 0), NameRole).toString());
					   }
					   emit columnsChanged(changedColumns);
			   }
	   }
	   
	emit dataSetChanged(); //For VariableInfoProvider and listeners
}

