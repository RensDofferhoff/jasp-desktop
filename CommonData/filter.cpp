#include "log.h"
#include <cassert>
#include "filter.h"
#include <atomic>

// The excision, Cut 3: DatabaseInterface is gone; Filter ids are minted from this
// process-global counter (computed datasets key their input by defaultInputFilterId).
static std::atomic<int> g_nextFilterId{1};
#include "timers.h"
#include "qutils.h"
#include "dataset.h"
#include "dataenums.h"
#include "filtereddata.h"
#include "columnencoder.h"
#include "columninfo.h"
#include "jsonutilities.h"
#include "varinfomodelproxy.h"
// The excision, Cut 5: labelfiltergenerator.h died with Label/Column — the generated-filter
// machinery returns with the labels editor (B2) on the jasp:labels overlay.

Filter::Filter(DataSet * data)
: DataSetBaseNode(dataSetBaseNodeType::filter, data),
  VariableInfoProvider(this),
  _data(				data), 
  _name(				DEFAULT_FILTER_NAME),
  _constructorJson(		DEFAULT_FILTER_JSON),
  _generatedFilter(		DEFAULT_FILTER_GEN)
{ 
	assert(_data);
	
	// The excision, Cut 3: the sqlite filter row is gone — mint the id locally.
	_id					= g_nextFilterId++;
	_rFilter			= fq(defaultRFilter());
	// The excision, Cut 5: _labelGen (LabelFilterGenerator) died with Label/Column — named
	// filters carry their own rFilter/constructorR instead; the generated filter stays the
	// passthrough DEFAULT_FILTER_GEN until the labels editor returns (B2).
	//_labelGen			= new LabelFilterGenerator(this);

	connectionCreation();
}

Filter::Filter(DataSet * data, const std::string & name, bool createIfMissing)
: DataSetBaseNode(dataSetBaseNodeType::filter),
  VariableInfoProvider(this),
  _data(data), _name(name)
{
	assert(_name != "");
	assert(_data);
	
	_rFilter			= fq(defaultRFilter()); //Might get overwritten, that is fine
	_generatedFilter	= DEFAULT_FILTER_GEN;
	
	// The excision, Cut 3: was a sqlite lookup (exists ? dbLoad : createIfMissing ? dbCreate : throw).
	// Filters live in memory owned by their DataSet; a fresh one always mints a fresh id.
	if(!createIfMissing)
		throw std::runtime_error("Filter by name '" + _name + "' but it doesnt exist and createIfMissing=false!\nAre you sure this filter should exist?");
	_id = g_nextFilterId++;
	
	//Named filters intentionally do NOT create a LabelFilterGenerator: it is only needed for the
	//(single, unnamed) default filter, which owns the label-level filtering generated from the
	//label checkboxes. Named filters carry their own rFilter/constructorR instead, so _labelGen
	//stays null for them and setConstructorR() below falls back to DEFAULT_FILTER_GEN.
	//_labelGen			= new LabelFilterGenerator(this);

	connectionCreation();
}


void Filter::connectionCreation()
{
	connect(this,	&Filter::dataSetShouldRefresh,	_data,	&DataSet::refresh			);
	connect(this,	&Filter::refreshAllAnalyses,	_data,	&DataSet::refreshAllAnalyses);
	connect(this,	&Filter::refreshAllCompCols,	_data,	&DataSet::refreshAllCompCols);
	connect(_data,	&DataSet::datasetChanged,		this,	&Filter::datasetChanged		);
	connect(this,	&Filter::nameChanged,			_data,	[&](){_data->incRevision();});
		
	_data->registerFilter(this);
	
		//NOTE: no longer relaying the default filter's generatedFilterChanged to named filters: it
		//fired a signal whose value was unchanged (the named filter's own generatedFilter is separate),
		//so it only caused spurious refreshes, and nothing consumes Filter::generatedFilterChanged.
	
	
	connect(_data,	&DataSet::labelsReordered,				infoSignaller(),	&VarInfoSignaller::labelsReordered			);
	connect(this,	&Filter::modelReset,					infoSignaller(),	&VarInfoSignaller::refresh					);
	
	// The excision, Cut 5: the columnTypeChanged lambda dereffed a Column* (gone) — serve the
	// schema type instead; labelChanged(const Column*) died with Column.
	connect(data(),			&DataSet::columnTypeChanged,				infoSignaller(),	[this](QString name){ const ColumnInfo * col = data() ? data()->schemaColumn(fq(name)) : nullptr; infoSignaller()->variableTypeChanged(name, col ? col->type : columnType::unknown); });
	connect(data(),			&DataSet::datasetChanged,					infoSignaller(),	&VarInfoSignaller::dataSetChanged			);
	connect(data(),			&DataSet::emptyValuesChanged,				infoSignaller(),	&VarInfoSignaller::dataSetChanged			);
	connect(data(),			&DataSet::modelReset,						infoSignaller(),	&VarInfoSignaller::refresh					);
	connect(data(),			&DataSet::dataChanged,						infoSignaller(),	&VarInfoSignaller::refresh					);
	

	connect(this,			&Filter::columnsInserted,					varInfo(),			&VariableInfo::rowCountChanged		);
	connect(this,			&Filter::columnsRemoved,					varInfo(),			&VariableInfo::rowCountChanged		);
	
}


bool Filter::setFilterVector(const boolvec & filterResult)
{
	bool changed = false;

	//The engine result is authoritative for the whole (current) dataset, so the cached vector must
	//match its length exactly. Only the first call may hit the empty-cache fast path; afterwards we
	//resize to the result length (grow with filtered=false / shrink) instead of stopping at the old
	//size, which previously dropped new rows and kept stale tail rows.
	if(_filtered.size() == 0)
	{
		_filtered = filterResult;
		changed = true;
	}
	else
	{
		if(_filtered.size() != filterResult.size())
		{
			_filtered.resize(filterResult.size());
			changed = true;
		}

		for(size_t i=0; i<filterResult.size(); i++)
			if(_filtered[i] != filterResult[i])
			{
				changed = true;
				_filtered[i] = filterResult[i];
			}
	}

	// The excision, Cut 3: the sqlite filter-vector write is gone.

	calculateFilteredRowCount();

	if(changed)
		incRevision();

	return changed;
}

void Filter::setFilterValueNoDB(size_t row, bool val)
{
	_filtered[row] = val;
}

void Filter::setRowCount(size_t rows)
{
	// The excision, Cut 5: the mask is load-bearing metadata now (FilteredData's
	// filterAcceptsRow + the forms' provider chain key off it). New rows arrive UNFILTERED
	// (v1 has no filter compaction) — std::vector<bool> value-initializes false, so the
	// fill must be explicit.
	size_t oldSize = _filtered.size();
	_filtered.resize(rows);
	if (rows > oldSize)
		std::fill(_filtered.begin() + oldSize, _filtered.end(), true);
	calculateFilteredRowCount();
}


void Filter::incRevision()
{
	assert(_id != -1);
	
	if(!_data->writeBatchedToDB())
	{
		_revision++;	// was db().filterIncRevision (the excision, Cut 3)
		checkForChanges();
	}
}

bool Filter::checkForUpdates()
{
	// The excision, Cut 3: was the sqlite revision diff-poll. Nothing external can mutate
	// this Filter anymore — there is never anything to update.
	return false;
}

void Filter::setName(const std::string &name)
{
	//"---" is the separator sentinel used by the filter dropdown lists; a real filter must never take
	//this name or it would be indistinguishable from a separator (and unselectable/ambiguous there).
	if(name == "---")
		return;

	bool	wasChange	=_name != name;
			_name		= name;

	incRevision();	// was dbUpdate() (the excision, Cut 3)

	if(wasChange)
		emit nameChanged();
}

void Filter::setRFilter(const std::string &rFilter)
{
	bool	wasChange	=_rFilter != rFilter;
			_rFilter	= rFilter;

	rescanForColumns();

	incRevision();	// was dbUpdate() (the excision, Cut 3)

	if(wasChange)
	{
		emit rFilterChanged();
		setInvalidated(true);
	}
}

void Filter::calculateFilteredRowCount()
{
	int newRowCount = 0;
	for(bool f : _filtered)
		if(f)
			newRowCount++;

	bool wasChange = newRowCount != _filteredRowCount;
	_filteredRowCount = newRowCount;

	if(wasChange)
		emit filteredRowCountChanged();
}

void Filter::setGeneratedFilter(const std::string &generatedFilter)
{
	bool	wasChange			=_generatedFilter != generatedFilter;
			_generatedFilter	= generatedFilter;

	incRevision();	// was dbUpdate() (the excision, Cut 3)

	if(wasChange)
	{
		setInvalidated(true);
		emit generatedFilterChanged();
	}
}


void Filter::setConstructorJson(const std::string &constructorJson)	
{ 
	bool	wasChange					=_constructorJson != constructorJson;
			_constructorJson			= constructorJson;

	rescanForColumns();

	incRevision();	// was dbUpdate() (the excision, Cut 3) 
	
	

	if(wasChange)
	{
		setInvalidated(true);
		emit constructorJsonChanged();
	}
}

void Filter::setConstructorR(const std::string &constructorR)
{
	bool	wasChange		=_constructorR != constructorR;
			_constructorR	= constructorR;

	// The excision, Cut 5: _labelGen is gone — the constructorR passthrough applies always.
	_generatedFilter = _constructorR == "" ? DEFAULT_FILTER_GEN : "generatedFilter <- " + _constructorR;
			
	incRevision();	// was dbUpdate() (the excision, Cut 3)
	
	if(wasChange)
	{
		setInvalidated(true);
		emit constructorRChanged();
	}
	
}

void Filter::setInvalidated(bool invalidated)
{
	bool	wasChange		=_invalidated != invalidated;
			_invalidated	= invalidated;

	incRevision();	// was dbUpdate() (the excision, Cut 3)

	if(wasChange)
		emit invalidatedChanged();
	
	if(_invalidated)
		emit _data->sendFilterByName(data()->id(), nameQ(), "*");
}

void Filter::setErrorMsg(const std::string &errorMsg)
{
	bool	wasChange	= _errorMsg != errorMsg;
			_errorMsg	= errorMsg;

	// was dbUpdateErrorMsg() — the sqlite write died with DatabaseInterface (the excision, Cut 3)
	incRevision();

	if(wasChange)
		emit filterErrorMsgChanged();
}

stringset Filter::columnsUsedInConstructor() const
{
	return _columnsInConstructorJson;
}

stringset Filter::columnsUsedInRFilter() const
{
	return _columnsUsedInRFilter;
}

bool Filter::filterNameIsFree(const std::string &filterName, DataSet * dataSet)
{
	if(!dataSet)
		return true;

	// The excision, Cut 3: was a sqlite lookup; the name is free iff no in-memory Filter has it.
	return dataSet->filter(filterName) == nullptr;
}

void Filter::reset()
{
	// The excision, Cut 3: was `if(!writeBatchedToDB()) { db writes }` — the sqlite branch is gone.
	incRevision();
	_filtered = boolvec(_data->rowCount(), true);
	calculateFilteredRowCount();
}


FilteredData *Filter::rowFilteredData()
{
	if(!_rowFilteredData)
	{
		_rowFilteredData = new FilteredData(this);
		_rowFilteredData->setSourceModel(_data);
	}
	
	return _rowFilteredData;
}

VarInfoModelProxy *Filter::rowFilteredVarInfo()
{
	if(!_rowFilteredVarInfo)
		_rowFilteredVarInfo = new VarInfoModelProxy(rowFilteredData());
	
	return _rowFilteredVarInfo;
}

FilteredData *Filter::rowFilteredData() const
{
	return _rowFilteredData;
}

VariableInfo *Filter::varInfo() const
{
	return _varInfo;
}

VariableInfo *Filter::varInfo()
{
	if(!_varInfo)
		_varInfo = new VariableInfo(this);
	
	return _varInfo;
}

VarInfoModelProxy *Filter::rowFilteredVarInfo() const
{
	return _rowFilteredVarInfo;
}

QAbstractItemModel *Filter::providerModel()
{
	return rowFilteredVarInfo();
}


QVariant Filter::provideInfo(varInfoType info, const QString& colName, int row) const
{
	try
	{
		// The excision, Cut 5: lane-bound datasets have no legacy Columns for the
		// FilteredData/VarInfoModelProxy machinery to read — the wire schema is the truth.
		// Same lane convention as ColumnsModel::provideInfo (data-model-design.md §3.4):
		// scale levels ≡ distinctCount, categorical levels ≡ the dictionary, numeric values
		// per numeric_levels. (A preview of Cut 6's provider cutover, done minimally.)
		if (data()->isOpen() && !colName.isEmpty())
		{
			const ColumnInfo * col = data()->schemaColumn(fq(colName));

			switch(info)
			{
			case varInfoType::VariableType:			return	int(!col ? columnType::unknown : col->type);
			case varInfoType::TotalLevels:			return	!col ? 0 : (col->type == columnType::scale ? int(col->distinctCount) : int(col->levels.size()));
			case varInfoType::TotalNumericValues:	return	!col ? 0 : (col->type == columnType::scale ? int(col->distinctCount) : col->numericLevels);
			case varInfoType::Labels:
			{
				if (!col)	return	QStringList();
				QStringList levels;
				for (const std::string & level : col->levels)
					levels.append(tq(level));
				return levels;
			}
			case varInfoType::ColumnDescription:	return	tq(col ? col->description : "");
			default:								break;
			}
		}

		switch(info)
		{
		case varInfoType::VariableNames:			return	tq(data()->getColumnNames());
		case varInfoType::DataSetRowCount:			return  rowFilteredData()->rowCount();
		case varInfoType::DataAvailable:			return	bool(data());
		case varInfoType::DataSetPointer:			return	QVariant::fromValue<void*>(data());
		default:									break;
		}

		int colIndex = data()->getColumnIndex(fq(colName));
		if (colIndex < 0)
			return QVariant();

		QModelIndex qColIndex	= index(colIndex, 0),
					tableCIndex	= rowFilteredData()->index(0, colIndex),
					tableVIndex	= rowFilteredData()->index(row, colIndex);

		switch(info)
		{
		case varInfoType::VariableType:				return	rowFilteredVarInfo()	->data(qColIndex, VarInfoModelProxy::ColumnTypeRole).toInt();
		case varInfoType::NameRole:					return	rowFilteredVarInfo()	->data(qColIndex, VarInfoModelProxy::NameRole);

		case varInfoType::DoubleValues:				return	rowFilteredData()		->	data(tableCIndex,						int(dataPkgRoles::valuesDblList));
		case varInfoType::TotalNumericValues:		return	rowFilteredData()		->	data(tableCIndex,						int(dataPkgRoles::nonFilteredNumericValuesCount));
		case varInfoType::TotalLevels:				return	rowFilteredData()		->	data(tableCIndex,						int(dataPkgRoles::nonFilteredLevels)).toStringList().length();
		case varInfoType::Labels:					return	rowFilteredData()		->	data(tableCIndex,						int(dataPkgRoles::nonFilteredLevels));
		case varInfoType::DataSetValues:			return	rowFilteredData()		->	data(tableCIndex,						int(dataPkgRoles::valuesStrList));
		case varInfoType::DataSetValue:				return	rowFilteredData()		->	data(tableVIndex,						int(dataPkgRoles::value));

		case varInfoType::MaxWidth:					return	rowFilteredData()		->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::maxColString)).toInt();
		case varInfoType::PreviewScale:				return	rowFilteredData()		->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::previewScale));
		case varInfoType::PreviewOrdinal:			return	rowFilteredData()		->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::previewOrdinal));
		case varInfoType::PreviewNominal:			return	rowFilteredData()		->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::previewNominal));
		case varInfoType::ColumnDescription:		return	rowFilteredData()		->headerData(colIndex, Qt::Horizontal,	int(dataPkgRoles::description));
		case varInfoType::SignalsBlocked:			throw std::runtime_error("????");
		default:									break;
		}
	}
	catch(std::exception & e)
	{
		Log::log() << "AnalysisForm::requestInfo had an exception! " << e.what() << std::flush;
		throw e;
	}

	return QVariant();
}

bool Filter::absorbInfo(varInfoType info, const QString &colName, int row, QVariant value)
{
	try
	{
		int colIndex = data()->getColumnIndex(fq(colName));

		if (colIndex < 0)
			return false;

		QModelIndex qColIndex	= rowFilteredData()->index(0, colIndex),
					qValIndex	= rowFilteredData()->index(row, colIndex);

		switch(info)
		{
		default:										return	false;
		case varInfoType::DataSetValue:					return	rowFilteredData()->setData(qValIndex, value,	int(dataPkgRoles::value));
		case varInfoType::DataSetValues:				return	rowFilteredData()->setData(qColIndex, value,	int(dataPkgRoles::valuesStrList));
		}
	}
	catch(std::exception & e)
	{
		Log::log() << "AnalysisForm::requestInfo had an exception! " << e.what() << std::flush;
		throw e;
	}

	return false;
}

void Filter::rescanForColumns()
{
	_columnsUsedInRFilter		= data()->findUsedColumnNames(_rFilter);
	_columnsInConstructorJson	= JsonUtilities::convertDragNDropFilterJSONToSet(_constructorJson);
}

void Filter::datasetChanged(int, QStringList changedColumns, QStringList missingColumns, QMap<QString, QString> changeNameColumns, bool rowCountChanged, bool hasNewColumns)
{
	bool invalidateMe = rowCountChanged;

	if(!invalidateMe)
		for(const QString & changed : changedColumns)
			if(_columnsUsedInRFilter.count(fq(changed)) > 0 || _columnsInConstructorJson.count(fq(changed)) > 0)
			{
				invalidateMe = true;
				break;
			}

	auto iUseOneOfTheseColumns = [&](std::vector<std::string> cols) -> bool
	{
		for(const std::string & col : cols)
			if(_columnsUsedInRFilter.count(col) > 0 || _columnsInConstructorJson.count(col) > 0)
				return true;

		return false;
	};

	if(iUseOneOfTheseColumns(fq(changeNameColumns.keys())))
	{
		std::map<std::string, std::string> stdChangeNameCols(fq(changeNameColumns));

		invalidateMe = true;

		setRFilter(			ColumnEncoder::replaceColumnNamesInRScript(rFilter(),							stdChangeNameCols));
		setConstructorJson( JsonUtilities::replaceColumnNamesInDragNDropFilterJSONStr(constructorJson(),		stdChangeNameCols));
	}

	auto missingStd = fq(missingColumns);
	if(iUseOneOfTheseColumns(missingStd))
	{
		setRFilter(ColumnEncoder::removeColumnNamesFromRScript(rFilter(), missingStd));

		setConstructorJson( JsonUtilities::removeColumnsFromDragNDropFilterJSONStr( constructorJson(), missingStd));

		invalidateMe = false; //Actually, if stuff is removed from the filter it won't work will it now?

		//Just reset the filter result to everything true while the user gets the change to fix their now broken filter
		reset();

		emit refreshAllAnalyses(this);
		data()->resetFilterCounters();
		updateStatusBar();

		//The following errormsg is overwritten immediately but that is because constructorJson changed triggers qml which triggers (some vents later) a send event. So yeah...
		//Ill leave it here though because it would be nice to show this friendlier msg then "null not found"
		setFilterErrorMsgQ(tr("Some columns were removed from the data and your filter(s)!"));
	}

	if(invalidateMe)
	{
		//Keep the cached filter vector length in sync with the dataset: on a row-count change the
		//engine result is (re)computed asynchronously, so until it lands we must not serve a vector
		//that is shorter than the dataset (drops rows) or longer (reads stale tail rows).
		if(rowCountChanged)
		{
			_filtered.resize(_data->rowCount());
			calculateFilteredRowCount();
		}

		setInvalidated(true);
	}


	//Do stuff for variable info provider:
	
   if(! (missingColumns.size() > 0 || hasNewColumns))
   {
		   if (changeNameColumns.size() > 0)
				   emit infoSignaller()->variableNamesChanged(changeNameColumns);
		   else if (changedColumns.size() > 0 || rowCountChanged)
		   {
				   if (rowCountChanged)
				   {
						   changedColumns.clear();
						   for (int i = 0; i < rowFilteredVarInfo()->rowCount(); i++)
								   changedColumns.push_back(rowFilteredVarInfo()->data(index(i, 0), VarInfoModelProxy::NameRole).toString());
				   }
				   emit infoSignaller()->variablesChanged(changedColumns);
		   }
   }
		
}

int Filter::rowCount(const QModelIndex &parent) const
{
	return parent.isValid() ? 0 : filtered().size();
}

int Filter::columnCount(const QModelIndex &parent) const
{
	return parent.isValid() ? 0 : 1;
}

QVariant Filter::data(const QModelIndex &index, int role) const
{
	if(!index.isValid())
		return QVariant();
	
	
	if(index.row() >= rowCount() || index.column() >= columnCount())
		return QVariant(); // if there is no data then it doesn't matter what role we play
	
	switch(role)
	{
	case Qt::DisplayRole:									
	case int(dataPkgRoles::filter):							return QVariant(filtered()[index.row()]);
	}
	
	return QVariant();
}

const std::string &Filter::generatedFilter() const 
{ 
	return _generatedFilter;
}

QString Filter::constructorRQ() const
{
	return tq(constructorR());
}

QString Filter::rFilterQ() const
{
	return tq(rFilter());
}


QString Filter::nameQ() const
{
	return tq(name());
}

QString Filter::filterErrorMsgQ() const
{
	return tq(errorMsg());
}

QString Filter::generatedFilterQ() const
{
	return tq(generatedFilter());
}

QString Filter::constructorJsonQ() const
{
	return tq(constructorJson());
}

bool Filter::columnUsed(const QString &name) const
{
	return _columnsInConstructorJson.count(fq(name)) || _columnsUsedInRFilter.count(fq(name));
}

const QString & Filter::defaultRFilter()
{
	static QString defaultFilter;

	const QString forceTranslatedStuffToAlwaysBeAComment =
		tr(
			"Above you see the code that JASP generates for both value filtering and the drag&drop filter."					"\n"
			"This default result is stored in 'generatedFilter' and can be replaced or combined with a custom filter."		"\n"
			"To combine you can append clauses using '&': 'generatedFilter & customFilter & perhapsAnotherFilter'"			"\n"
			"Click the (i) icon in the lower right corner for further help."												"\n");

	defaultFilter = "# " + tq(stringUtils::replaceBy(fq(forceTranslatedStuffToAlwaysBeAComment), "\n", "\n# ") + "\n\ngeneratedFilter");

	return defaultFilter;
}

bool Filter::hasFilter() const
{
	return rFilter() != defaultRFilter() || constructorJson() != DEFAULT_FILTER_JSON; 
}

void Filter::setRFilterQ(const QString &newRFilter) 
{
	setRFilter(			fq(newRFilter));			
}

void Filter::setConstructorRQ(const QString &newConstructorR) 
{ 
	setConstructorR(	fq(newConstructorR));	
}

void Filter::setGeneratedFilterQ(const QString &newGeneratedFilter) 
{ 
	setGeneratedFilter(	fq(newGeneratedFilter));	
}

void Filter::setConstructorJsonQ(const QString &newconstructorJson) 
{ 
	setConstructorJson(		fq(newconstructorJson));	
}

void Filter::setFilterErrorMsgQ(const QString &newFilterErrorMsg) 
{ 
	setErrorMsg(fq(newFilterErrorMsg));		
}

void Filter::setStatusBarText(const QString &newStatusBarText)
{
	_statusBarText  = newStatusBarText;
}

void Filter::checkFilterResults()
{
	// The excision, Cut 3: the "load new filter values from the database" step
	// (dbLoadResultAndError) is gone — the in-memory _filtered IS the truth now.
}
