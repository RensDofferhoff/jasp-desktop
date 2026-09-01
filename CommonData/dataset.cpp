// Copyright (C) 2013-2026 University of Amsterdam
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public
// License along with this program.  If not, see
// <http://www.gnu.org/licenses/>.
//
#include "log.h"
#include <cassert>
#include "timers.h"
#include "qutils.h"
#include "dataset.h"
#include "appinfo.h"
#include "workspace.h"
#include "dataenums.h"
#include "columnencoder.h"
#include "jsonutilities.h"
#include "undostack.h"

#include <atomic>

// The excision, Cut 3: DatabaseInterface is gone. Dataset ids are minted from this
// process-global counter (they only key the workspace map, the encoder prefix and the
// undo/computed-column wiring — no sqlite rows exist anymore).
static std::atomic<int> g_nextDataSetId{1};

stringset DataSet::_defaultEmptyvalues;

DataSet::DataSet(Workspace * workspace)
	: DataSetBaseNode(dataSetBaseNodeType::dataSet, workspace),
	  _workspace(workspace)
{
	_encoder = new ColumnEncoder();
	_emptyValues	= new EmptyValues(nullptr);
	connect(_emptyValues,	&EmptyValues::emptyValuesChanged,	this,		&DataSet::emptyValuesChanged			);
	connect(this,			&DataSet::emptyValuesChanged,		_workspace, &Workspace::emptyValuesChanged			);
	
	//Was dbCreate(): mint the identity in memory (a default Filter registers itself in _filters).
	_dataSetId		= g_nextDataSetId++;
	_defaultFilter	= new Filter(this);
	_rowCount		= 0;
	setupEncoderPrefix();
	Log::log() << "DataSet::DataSet(id=" << _dataSetId << ")" << std::endl;
	
	_undoStack = new UndoStack(this);
	
	connect(this,			&DataSet::datasetChanged,			this,		&DataSet::handleDataSetChanged			);
	
	connect(this,			&DataSet::showYesNo,				_workspace, &Workspace::showYesNo					);
	connect(this,			&DataSet::askPassword,				_workspace, &Workspace::askPassword					);
	connect(this,			&DataSet::showWarning,				_workspace, &Workspace::showWarning					);
	connect(this,			&DataSet::manualEditMade,			_workspace, &Workspace::manualEditMade				);
	connect(this,			&DataSet::datasetChanged,			_workspace, &Workspace::datasetChanged				);
	connect(this,			&DataSet::labelsReordered,			_workspace, &Workspace::labelsReordered				);

	connect(this,			&DataSet::somethingModified,		_workspace, &Workspace::enableModified				);
	connect(this,			&DataSet::sendFilter,				_workspace, &Workspace::sendFilter					);
	connect(this,			&DataSet::sendFilterByName,			_workspace, &Workspace::sendFilterByName			);
	connect(this,			&DataSet::filtersCountChanged,		_workspace, &Workspace::filtersCountChanged			);
	connect(this,			&DataSet::refreshAllAnalyses,		_workspace, &Workspace::refreshAllAnalyses			);
	connect(this,			&DataSet::refreshAllCompCols,		_workspace, &Workspace::refreshAllCompCols			);
	
	connect(_workspace,		&Workspace::filterByNameDone,		this,		&DataSet::filterByNameDone				);

	setTitle(name().replace("_", " "));

	_description = fq(tr("Originally created empty by %1 on %2").arg(tq(AppInfo::getShortDesc())).arg(tq(Utils::currentDateTime())));
}

DataSet::~DataSet()
{
	JASPTIMER_SCOPE(DataSet::~DataSet);

	//If this dataset's encoder is the one currently active (shown), make sure the indirection doesn't
	//keep pointing at it once it's freed. Otherwise later encode/decode would dereference freed memory.
	if(ColumnEncoder::currentEncoder() == _encoder)
		ColumnEncoder::setCurrentEncoder(nullptr);

	delete _encoder;
	_encoder = nullptr;

	delete _emptyValues;
	
	for(Filter * f : _filters)
		unregisterNode(f);
		
	_emptyValues	= nullptr;
	_defaultFilter	= nullptr;
}

void DataSet::deleteShownFilter()
{
	if(_shownFilter != _defaultFilter)
		removeFilter(_shownFilter);
}

void DataSet::addFilter()
{
	std::string filterName;
	
	int filterId = 0;
	do
	{
		filterName = fq(tr("Filter %1").arg(filterId++));
	}
	while(filterExists(filterName));
	
	showFilter(createFilter(filterName));
}

void DataSet::showFilter(Filter * f)
{
	if(f->data() != this)
		return;
	
	_shownFilter = f;
	emit shownFilterChanged(this);
	refresh();	
}

Filter * DataSet::showFilter(const std::string &filterName)
{
	if(filterName == "")
	{
		_shownFilter = nullptr;
		return nullptr;
	}
	
	Filter * found = filter(filterName);;
	
	try
	{
		if(!found)
			found = new Filter(this, filterName, false);
	}
	catch(...){}
	
	if(!found)
		found = defaultFilter();
	
	if(found && found != _shownFilter)
		showFilter(found);
	
	return found;
}
	

Filter * DataSet::showFilter(const QString &filterName)
{
	return showFilter(fq(filterName));
}

QString DataSet::name() const
{
	// The excision, Cut 3: the name used to live in sqlite (db().dataSetName(id)) — it is
	// generated from the id now; the user-visible label is title() anyway.
	return "Dataset " + QString::number(id());
}

QString DataSet::title() const
{
	return _title.empty() ? name().replace("_", " ") : tq(_title);
}


Filter * DataSet::filter(const std::string &name)
{
	for(Filter * f : _filters)
		if(f->name() == name)
			return f;
	
	return _defaultFilter && _defaultFilter->name() == name ? _defaultFilter : nullptr;
}

Filter *DataSet::filter(int id)
{
	for(Filter * f : _filters)
		if(f->id() == id)
			return f;
	
	return _defaultFilter && _defaultFilter->id() == id ? _defaultFilter : nullptr;
}

void DataSet::registerFilter(Filter *f)
{
	_filters.push_back(f);
	emit filtersCountChanged();
}

void DataSet::removeFilter(Filter *f)
{
	if(!f || f == _defaultFilter)
		return;

	const int removedId = f->id();

	//Computed datasets that used this filter as their input would otherwise keep a dangling
	//defaultInputFilterId. Clear it and surface an error so the user knows why the computed dataset
	//can no longer be produced.
	if(removedId > 0 && _workspace)
		for(DataSet * ds : _workspace->dataSets())
			if(ds != this && ds->isComputed() && ds->defaultInputFilterId() == removedId)
			{
				ds->setDefaultInputFilterId(-1);
				ds->setError("The filter used as input for this computed dataset was removed.");
			}

	//If the removed filter is the shown one, pick a surviving replacement so _shownFilter never dangles.
	bool wasShown = _shownFilter == f;
	size_t indexWas = 0;

	Filters newList;
	for(size_t i=0; i<_filters.size(); i++)
		if(_filters[i] != f)
			newList.push_back(_filters[i]);
		else
			indexWas = i;

	_filters = newList;

	emit filterRemoved(f);
	emit filtersCountChanged();

	if(wasShown)
	{
		_shownFilter = _filters.empty() ? _defaultFilter : _filters[std::min(indexWas, _filters.size() - 1)];
		emit shownFilterChanged(this);
		incRevision();
		refresh();
	}

	delete f;
}

void DataSet::dbDelete()
{
	JASPTIMER_SCOPE(DataSet::dbDelete);
	
	assert(_dataSetId != -1);
	
	for(Filter * f : _filters)
		delete f;	// The excision, Cut 3: purely in-memory teardown now (Filter dtor unregisters)
	
	_filters.clear();

	_dataSetId = -1;
}

void DataSet::beginBatchedToDB()
{
	_writeBatchedToDBDepth++;
}

void DataSet::endBatchedToDB(std::function<void(float)> progressCallback)
{
	if(_writeBatchedToDBDepth > 0)
		_writeBatchedToDBDepth--;
	
	if(_writeBatchedToDBDepth == 0)
	{
		// The excision, Cut 3: the batched sqlite write is gone; keep the useful side effects
		// (encoder resync + revision bump so consumers refresh).
		progressCallback(1);

		//Column names/types have (just) been (re)loaded into this DataSet, so keep our own encoder in
		//sync; when this dataset is the shown/current one it is what the encoder indirection points at.
		_encoder->setCurrentNames(getColumnNames());

		incRevision(); //Should trigger reload at engine end
	}
}

int DataSet::getColumnIndex(const std::string & name) const
{
	// The excision, Cut 5: served the legacy _columns order — the schema is the one home now.
	return schemaColumnIndex(name);
}


stringvec DataSet::getColumnNames()
{
	stringvec names;

	// The schema is the column truth (the excision, Cut 5). This is what the analysis-form
	// provider chain serves as VariableNames (Filter::provideInfo): an empty list here =
	// "no variables" in every opened analysis form.
	for (const ColumnInfo & col : _schemaColumns)
		names.push_back(col.name);

	return names;
}


std::map<std::string,columnType> DataSet::getColumnTypesMap()
{
	std::map<std::string,columnType> theMap;

	//The excision, Cut 2: the encoder's name set must also come from the schema when
	//lane-bound — `encode()` validates against this map (a lane open with a stale/empty
	//map made every schema name "not a columnName").
	for (const ColumnInfo & col : _schemaColumns)
		theMap[col.name] = col.type;

	return theMap;
}

void DataSet::setupEncoderPrefix()
{
	//Make the encoder prefix globally unique (carries the dataset id) so ALL datasets loaded into the
	//engine can coexist without encoded-name collisions. Must run after the id has been finalized
	//(dbCreate/dbLoad), which is why it is called at the end of those, not in the constructor.
	_encoder->_encodePrefix = "JASPColumn_" + std::to_string(_dataSetId) + "_";
	_encoder->setCurrentNames(getColumnTypesMap()); //regenerate all encoded names with the new prefix
}

void DataSet::setDataFileAndTimeStamp(const std::string &dataFilePath, long timestamp)
{
	bool isChange		= _dataFilePath	!= dataFilePath || _dataFileTimestamp	!= timestamp;
	_dataFileTimestamp	= timestamp;		
	_dataFilePath		= dataFilePath;
	if(isChange) incRevision(); 
	
	if(isChange)
	{
		emit dataFileChanged();
		emit dataTimestampChanged();
	}
}

void DataSet::setDataFile(const std::string &dataFilePath)	
{ 
	bool isChange	= _dataFilePath	!= dataFilePath;
	_dataFilePath	= dataFilePath;
	if(isChange) incRevision(); 
	
	if(isChange)
		emit dataFileChanged();
}

void DataSet::setDataTimestamp(long timestamp)						
{ 
	bool isChange		= _dataFileTimestamp	!= timestamp;
	_dataFileTimestamp	= timestamp;		
	if(isChange) incRevision(); 
	
	if(isChange)
		emit dataTimestampChanged();
}

void DataSet::setDatabaseJson(const Json::Value & databaseJson)
{ 

	bool isChange	= _database	!= databaseJson;
	_database	= databaseJson;
	if(isChange) incRevision(); 
	
	if(isChange)
		emit databaseJsonChanged(); 
}

void DataSet::setDataFileSynch(bool synchronizing)					
{ 
	bool isChange	= _dataFileSynch	!= synchronizing;
	_dataFileSynch	= synchronizing;	
	if(isChange) incRevision(); 
	
	if(isChange)
		emit dataFileSynchChanged();
}

// The excision, Cut 3: dbCreate moved into the ctor (id from the process-global counter);
// dbUpdate became inline `incRevision()` at its callers; dbLoad (the .jasp/sqlite restore
// path) is deleted wholesale — .jasp persistence returns in a later NEO era.


void DataSet::upgradeEmptyValsFrom018To019(const Json::Value & emptyVals)
{
	//So, 0.18.0, 0.18.1, 0.18.2 jaspfiles cant be loaded in 0.18.3
	//also, those versions were pretty buggy, so here we will just try to handle the case of 0.18.3
	//above we made sure _ints and _dbls are synched again.
	//now we will extract the missing data map and turn it into emptyvalues and proper values
	
	// The emptyValues json contains
	const Json::Value	& emptyValuesPerColumn = emptyVals["emptyValuesPerColumn"], // object, names=columnnames: array of empty value strings
						& missingDataPerColumn = emptyVals["missingDataPerColumn"], // object, names=columnnames: object { "row#": "original display" }
						& workspaceEmptyValues = emptyVals["workspaceEmptyValues"]; // array of empty value strings
	
	Log::log() << "Upgrading empty values from 0.18 to higher looked at jsons:\nemptyValuesPerColumn: " << emptyValuesPerColumn.toStyledString() << "\nmissingDataPerColumn: " << missingDataPerColumn.toStyledString() << "\nworkspaceEmptyValues: " << workspaceEmptyValues.toStyledString() << std::endl;
	
	stringset workspaceEmpty = JsonUtilities::jsonStringArrayToSet(workspaceEmptyValues);
	
	// The excision, Cut 5: the per-Column empty-values reconciliation walked the legacy
	// Columns — gone. Only the workspace-level set survives.
	_emptyValues->setEmptyValues(workspaceEmpty);
	
	
	Log::log() << "Based on this the new workspace emtpy values are:\n" << _emptyValues->toJson().toStyledString() << std::endl;
	
	incRevision();	// was dbUpdate() (the excision, Cut 3)
}

void DataSet::setRowCountMetadata(size_t rowCount)
{
	// The LANE row count (applySchema/applyRevision): metadata ONLY — never materialize
	// per-row value storage (gigabytes of dead weight on a large lane dataset; the grid
	// reads cells through the view lane).
	_rowCount = rowCount;

	// The default filter's per-row mask, though, is LOAD-BEARING metadata: FilteredData's
	// filterAcceptsRow, getRowFilter and — via Filter::rowCount = filtered().size() — every
	// QModelIndex the forms' provider chain (Filter::provideInfo → VarInfoModelProxy) mints.
	// With the mask empty, Filter::index(colIndex, 0) is invalid and VariableType lookups
	// degrade to unknown (the min/max-levels class of bugs). v1 has no filter compaction:
	// the mask is all-true at the dataset's extent (resize default-constructs true).
	bool	rowDelta	= _rowCount != int(rowCount);
	_rowCount		= int(rowCount);

	if (_defaultFilter)
		_defaultFilter->setRowCount(size_t(rowCount));

	// And because views/proxies may ALREADY be attached (the provider fixture attaches
	// FilteredData before the schema lands), the new extent must be ANNOUNCED — a bare int
	// write never reaches a QSortFilterProxyModel's mapping. A reset is honest: this runs on
	// open and revision landings, both of which restart the consuming views anyway.
	if (rowDelta)
	{
		beginResetModel();
		endResetModel();
	}
}

void DataSet::incRevision()
{
	assert(_dataSetId != -1);

	if(!writeBatchedToDB())
	{
		_revision++;	// was db().dataSetIncRevision (the excision, Cut 3)
		checkForChanges();
	}
}

bool DataSet::checkForUpdates(std::function<void(float)> progressCallback)
{
	// The excision, Cut 3: this was the sqlite diff-poll (revision compare, dbLoad refresh,
	// filter-row reconciliation). With DatabaseInterface gone nothing external can mutate this
	// DataSet, so there is never anything to update.
	(void) progressCallback;
	return false;
}

void DataSet::runComputedDataset(QString code, int defaultInputFilterId)
{
	emit _workspace->runComputedDataSet(id(), code, defaultInputFilterId);
}

std::string DataSet::rCodeStripped() const
{
	return stringUtils::stripRComments(_rCode);
}

Filter * DataSet::defaultInputFilter() const
{
	return _workspace ? _workspace->filterById(_defaultInputFilterId) : nullptr;
}

DataSet * DataSet::defaultInputDataSet() const
{
	Filter * input = defaultInputFilter();
	return input ? input->data() : nullptr;
}

bool DataSet::iShouldBeSentAgain()
{
	if(!invalidated())
		return false;

	DataSet * input = defaultInputDataSet();

	if(input && input->isComputed() && input->invalidated())
		return false;

	return true;
}

void DataSet::dbUpdateComputedDatasetStuff()
{
	std::string oldError = _error;

	// The excision, Cut 3: the sqlite computed-info write died with DatabaseInterface.
	incRevision();

	if(oldError != _error)
		emit errorChanged();
}

bool DataSet::setRCode(const std::string & rCode)
{
	if(_rCode == rCode)
		return false;

	_rCode		= rCode;
	invalidate();
	dbUpdateComputedDatasetStuff();
	emit rCodeChanged();
	checkForDependentDatasetsToBeSent(true);

	return true;
}

void DataSet::setCodeType(computedColumnType codeType)
{
	if(codeType == _codeType)
		return;

	_codeType = codeType;

	dbUpdateComputedDatasetStuff();
	emit codeTypeChanged();
}

void DataSet::setInvalidated(bool invalidated)
{
	if(_invalidated == invalidated)
		return;

	_invalidated = invalidated;
	// The excision, Cut 3: the sqlite computed-info write died with DatabaseInterface.
	incRevision();
	emit invalidatedChanged();
}

bool DataSet::setError(const std::string & error)
{
	if(error == _error)
		return false;

	_error = error;
	dbUpdateComputedDatasetStuff();

	//dbUpdateComputedDatasetStuff() snapshots oldError *after* we already changed _error, so it can't
	//detect this change itself: emit explicitly (mirrors Column::setError) so QML's `error` binding
	//gets notified that a computed dataset failed.
	emit errorChanged();

	return true;
}

bool DataSet::setDefaultInputFilterId(int defaultInputFilterId)
{
	if(defaultInputFilterId == _defaultInputFilterId)
		return true;

	//Refuse to introduce a cycle (A <- B <- A) between computed datasets: a computed dataset must
	//not depend on an input filter that (transitively) depends on it, or the recompute cascade would livelock.
	if(_workspace && defaultInputFilterId >= 0)
	{
		Filter * inputFilter = _workspace->filterById(defaultInputFilterId);
		DataSet * target = inputFilter ? inputFilter->data() : nullptr;
		if (target && _workspace->wouldCreateComputedDataSetLoop(this, target))
		{
			setError("The filter chosen as input for this computed dataset would create a loop between the computed datasets.");
			dbUpdateComputedDatasetStuff();
			return false;
		}
	}

	_defaultInputFilterId = defaultInputFilterId;
	invalidate();

	//Clear any previously surfaced input-loop error: picking a valid input must not leave the earlier
	//"would create a loop" error persisting.
	dbUpdateComputedDatasetStuff();
	if(!_error.empty())
		setError("");

	emit defaultInputFilterChanged();

	//Changing only the *input* must still trigger a recompute (setRCode no-ops when the code text is
	//unchanged, so input-only edits used to leave the computed dataset stuck invalidated).
	checkForDependentDatasetsToBeSent(true);

	return true;
}

bool DataSet::tryAndRunComputedDataset()
{
	const std::string code = rCodeStripped();

	if(code.empty())
		return false;

	runComputedDataset(tq(code), _defaultInputFilterId);

	return true;
}

void DataSet::checkForDependentDatasetsToBeSent(bool refreshMe)
{
	//Invalidate this (if refreshMe) and every computed dataset that reads from this one.
	for(DataSet * ds : _workspace->dataSets())
		if(ds->isComputed() && ((ds == this && refreshMe) || ds->defaultInputDataSet() == this))
			ds->invalidate();

	//Anti-livelock guard: if the computed-dataset dependency graph somehow contains a cycle (e.g.
	//restored from an old file), do not keep requesting computations; mark the participants with an
	//error instead so the user breaks the circle.
	std::string loopError;
	if (_workspace->computedDataSetsHaveLoop(loopError))
	{
		for (DataSet * ds : _workspace->dataSets())
			if (ds->isComputed() && ds->invalidated())
				ds->setError(loopError);
		return;
	}

	//Re-dispatch only the datasets that were invalidated above (plus dependents). Crucially, this must
	//NOT re-dispatch `this` when refreshMe is false: handleDataSetChanged() (which runs on any data
	//reload, including the reload right after a successful compute) calls this with refreshMe=false and
	//`this` still invalidated at that moment; re-dispatching it there would recompute it forever.
	for(DataSet * ds : _workspace->dataSets())
		if(ds->isComputed() && ((ds == this && refreshMe) || ds->defaultInputDataSet() == this))
			if(ds->iShouldBeSentAgain())
				ds->tryAndRunComputedDataset();
}

void DataSet::setEmptyValuesJsonOldStuff(const Json::Value &emptyValues)
{
	// For backward compatibility we take the default ones if the workspaceEmptyValues are not specified
	Json::Value updatedEmptyValues = emptyValues;
	Json::Value emptyValuesJson(Json::arrayValue);
	for (const std::string& val : _defaultEmptyvalues)
		emptyValuesJson.append(val);
	updatedEmptyValues["workspaceEmptyValues"] = emptyValuesJson;
	_emptyValues->fromJson(updatedEmptyValues);
}

void DataSet::setEmptyValuesJson(const Json::Value &emptyValues, bool updateDB)
{
	try
	{
		if (emptyValues.isMember("workspaceEmptyValues"))
			setEmptyValuesJsonOldStuff(emptyValues);
		else
			_emptyValues->fromJson(emptyValues);
	}
	catch(std::exception & e)
	{
		Log::log() << "DataSet::setEmptyValuesJson got exception: " << e.what() << std::endl;
	}

	if (updateDB)
		incRevision();	// was dbUpdate() (the excision, Cut 3)
}

void DataSet::setEmptyValuesFromStrings(const stringset &values)
{
	_emptyValues->setEmptyValues(values);
	incRevision();	// was dbUpdate() (the excision, Cut 3)
}

void DataSet::setDescription(const std::string &desc)
{
	bool isChange	= _description != desc;
	_description	= desc;
	incRevision();	// was dbUpdate() (the excision, Cut 3)
	
	if(isChange)
		emit descriptionChanged();
}

void DataSet::refresh(bool doColumnsToo)	
{
	Q_UNUSED(doColumnsToo);	// the excision, Cut 5: there are no legacy Columns to refresh

	beginResetModel();
	endResetModel(); 

	//Emit these after the reset completes: they connect into models that may re-query this DataSet,
	//which must not happen while a reset is still in progress.
	emit descriptionChanged();
	emit dataFileChanged();
	emit databaseJsonChanged();
	emit dataFileSynchChanged();
	emit dataTimestampChanged();
	emit columnsLabelFilteredCountChanged();
	emit shownFilterChanged(this);
	emit titleChanged();
}

void DataSet::runFilters()
{
	_defaultFilter->setInvalidated(true);
	
	for(Filter * f : _filters)
		f->setInvalidated(true);
}

// The excision, Cut 3: DataSet::db() died with DatabaseInterface.

stringset DataSet::findUsedColumnNames(std::string searchThis)
{
	stringset columnsFound, columnsWithTypeFound;
	encoder().encodeRScript(searchThis, &columnsWithTypeFound);
	
	//The found columns now also include the type, but we dont really care about that right now.
	//Instead we'll make use of the encode->decode not being symmetrical (for the results to be less ugly) and dropping the type
	
	for(const std::string & colPlusType : columnsWithTypeFound)
		columnsFound.insert(encoder().decode(encoder().encode(colPlusType)));
	
	return columnsFound;
}

int DataSet::rowCount(const QModelIndex &) const
{
	return _rowCount;
}

int DataSet::columnCount(const QModelIndex &) const
{
	// The schema is the column truth (the excision, Cut 5; the legacy mirror served a
	// grow-only count that went stale after delete_cols edits).
	return int(_schemaColumns.size());
}

QVariant DataSet::data(const QModelIndex &index, int role) const
{
	if(!index.isValid())
		return QVariant();
	
	
	if(index.row() >= rowCount() || index.column() >= columnCount())
		return QVariant(); // if there is no data then it doesn't matter what role we play
	
	JASPTIMER_SCOPE(DataSet::data);
	
	// The SCHEMA serves the metadata roles (the excision, Cut 5 — the legacy Column branch
	// is gone), and the analysis-form provider chain reads through this model API
	// (Filter → FilteredData → VarInfoModelProxy → provideInfo). Cell VALUES live in the
	// view lane, never here: the value/display roles serve honest empties.
	{
		const ColumnInfo * info = schemaColumnAt(size_t(index.column()));
		if (!info)
			return QVariant();
		switch(role)
		{
		case int(dataPkgRoles::name):							return tq(info->name);
		case int(dataPkgRoles::title):							return tq(info->displayName);
		case int(dataPkgRoles::columnType):					return int(info->type);
		case int(dataPkgRoles::description):					return tq(info->description);
		case int(dataPkgRoles::nonFilteredLevels):
		{
			// The levels list (column-constant through any row index — the wire's capped UI
			// prefix; distinct_count stays the truth for counts).
			QStringList levels;
			for (const std::string & level : info->levels)
				levels.append(tq(level));
			return levels;
		}
		case int(dataPkgRoles::nonFilteredNumericValuesCount):
			// ColumnsModel's lane convention: scale — every value numeric (distinctCount);
			// categorical — the wire's numeric_levels hint (never parsed on the frontend).
			if (info->type == columnType::scale)
				return qulonglong(info->distinctCount);
			return qulonglong(info->numericLevels);
		case int(dataPkgRoles::computedColumnType):			return int(computedColumnType::notComputed);
		case int(dataPkgRoles::columnPkgIndex):				return index.column();
		case int(dataPkgRoles::filter):						return true;// no filter compaction on lane (v1)
		default:											return QVariant();	// display/value/label/lines: the view lane owns cells
		}
	}
	
	return QVariant();
}

QVariant DataSet::headerData(int section, Qt::Orientation orientation, int role) const
{
	if (section < 0 || section >= (orientation == Qt::Horizontal ? columnCount() : rowCount()))
			return QVariant();
		
	JASPTIMER_SCOPE(DataSet::headerData);
	
	if(orientation == Qt::Vertical)
		switch(role)
		{
		default:
			return QVariant();

		case int(dataPkgRoles::maxRowHeaderString):
			return QString::number(rowCount()) + "XXX";

		case Qt::DisplayRole:
			return QVariant(section + 1);
			
		case int(dataPkgRoles::filter):
			return !(section >= 0 && shownFilter()->filtered().size() > 0) || shownFilter()->filtered()[section];
		}
	else
	{
		// Schema-served metadata (the excision, Cut 5 — the legacy Column branch is gone).
		const ColumnInfo * info = schemaColumnAt(size_t(section));
		if (!info)
			return QVariant();
		switch(role)
		{
		case Qt::DisplayRole:
		case int(dataPkgRoles::name):							return tq(info->name);
		case int(dataPkgRoles::title):							return tq(info->displayName);
		case int(dataPkgRoles::columnType):						return int(info->type);
		case int(dataPkgRoles::description):					return tq(info->description);
		case int(dataPkgRoles::computedColumnType):				return int(computedColumnType::notComputed);
		case int(dataPkgRoles::columnIsComputed):				return false;
		case int(dataPkgRoles::computedColumnError):			return QString();
		case int(dataPkgRoles::computedColumnIsInvalidated):	return false;
		case int(dataPkgRoles::filter):
		case int(dataPkgRoles::labelsHasFilter):				return false;
		case int(dataPkgRoles::maxColumnHeaderString):			return tq(info->name) + "XXX";
		case int(dataPkgRoles::maxColString):					return tq(info->displayName) + "XXXXXXX";	// a width estimate — no cell scan on lane
		case int(dataPkgRoles::maxRowHeaderString):				return QString::number(rowCount()) + "XXX";
		case Qt::TextAlignmentRole:							return QVariant(Qt::AlignCenter);
		default:											return QVariant();	// previews: honest none until data_view serves them
		}
	}
	
	return QVariant();
}

Qt::ItemFlags DataSet::flags(const QModelIndex &index) const
{
	Q_UNUSED(index);
	// The excision, Cut 5: editability is served by the NEO grid path (the proxy's gate);
	// this model is metadata-only, read-only.
	return Qt::ItemIsSelectable | Qt::ItemIsEnabled;
}

bool DataSet::isColumnNameFree(const std::string & name) const
{
	return getColumnIndex(name) == -1;	
}

// ————— Inert model write API (the excision, Cut 5) —————
// These QAbstractItemModel overrides were the legacy TableModel's write path (Column-value
// surgery). Structural change on lane is REMOTE (a revision lands via applyRevision), and
// cell edits go through DataEditCommand via the grid. Keep the vtable honest: nothing writes.

bool DataSet::setData(const QModelIndex &index, const QVariant &value, int role)
{
	Q_UNUSED(index); Q_UNUSED(value); Q_UNUSED(role);
	return false;
}

bool DataSet::insertRows(int row, int count, const QModelIndex &)
{
	Q_UNUSED(row); Q_UNUSED(count);
	return false;
}

bool DataSet::insertColumns(int column, int count, const QModelIndex &)
{
	Q_UNUSED(column); Q_UNUSED(count);
	return false;
}

bool DataSet::removeRows(int row, int count, const QModelIndex &)
{
	Q_UNUSED(row); Q_UNUSED(count);
	return false;
}

bool DataSet::removeColumns(int column, int count, const QModelIndex &)
{
	Q_UNUSED(column); Q_UNUSED(count);
	return false;
}

int DataSet::columnsLabelFilteredCount() const
{
	// Label filters lived on Columns (B2 rebuilds on the jasp:labels overlay) — always 0.
	return 0;
}

// The excision, Cut 5: freeNewColumnName named legacy Columns as they were created.

void DataSet::handleDataSetChanged( int							dataSetID,
									QStringList			changedColumns,
									QStringList			missingColumns,
									QMap<QString, QString>	changeNameColumns,
									bool				rowCountChanged,
									bool				hasNewColumns)
{
	assert(_dataSetId == dataSetID);

	// The excision, Cut 5: the computed-Column invalidation/rewrite walk died with Column.

	_encoder->setCurrentNames(	getColumnTypesMap());

	//Computed datasets that read from this dataset must be recomputed too.
	if(
		changedColumns.size()		||
		missingColumns.size()		||
		changeNameColumns.size()	||
		rowCountChanged				||
		hasNewColumns				
	)
		checkForDependentDatasetsToBeSent();

	
}





bool DataSet::getRowFilter(int row) const
{
	const Filter * filter = shownFilter();
	if(!filter)
		return true;

	const std::vector<bool> & filtered = filter->filtered();
	return filtered.empty() || (row >= 0 && static_cast<size_t>(row) < filtered.size() && filtered[row]);
}

QVariant DataSet::getDataSetViewLines(bool up, bool left, bool down, bool right)
{
	return			(left ?		1 : 0) +
					(right ?	2 : 0) +
					(up ?		4 : 0) +
					(down ?		8 : 0);
}

QString DataSet::descriptionQ() const
{
	return tq(description());
}

void DataSet::setDescriptionQ(const QString & newDescription)
{
	setDescription(fq(newDescription));
}

QString DataSet::dataFileQ() const
{
	return tq(dataFilePath());
}

void DataSet::setDataFileQ(const QString &newDataFile)
{
	setDataFile(fq(newDataFile));
}

void DataSet::setTitle(const QString &title)
{
	QString uniqueTitle = _workspace ? _workspace->makeDataSetTitleUnique(title, this) : title;

	if(_title == fq(uniqueTitle))
		return;

	_title = fq(uniqueTitle);

	emit titleChanged();

	if(_workspace)
		emit _workspace->dataSetTitleChanged(id());

	incRevision();	// was dbUpdate() (the excision, Cut 3)
}

void DataSet::resetAllFilters()
{
	// The excision, Cut 5: label filters lived on Columns — gone. Keep the signals so the
	// filter UI (QML) stays consistent.
	emit allFiltersReset();
	emit columnsLabelFilteredCountChanged();
}

void DataSet::resetFilterCounters()
{
	// The excision, Cut 5: label-filter counters lived on Columns — gone.
}


void DataSet::filterByNameDone(int dataSetID, const QString &name, const QString &error)
{
	//Every DataSet is connected to the shared Workspace::filterByNameDone; only act on completions
	//that target this dataset, otherwise a same-named filter in another dataset would be reloaded.
	if(dataSetID != id())
		return;

	// The excision, Cut 3: the "reload the filter result from the database" step
	// (f->dbLoadResultAndError) is gone — there is no sqlite to poll, so there is nothing
	// to refresh here (filter state changes propagate through their own signals).
	(void) name; (void) error;
}


void DataSet::applySchema(const std::string & datasetId, uint64_t rows, const Json::Value & schema, const std::string & sourcePath)
{
	_laneDatasetId	= datasetId;
	_laneRevision	= 0;		// a fresh identity starts a fresh revision space (the initial open is revision 0)
	landWireSchema(datasetId, rows, schema);
}

/// The shared landing of a wire schema (open via applySchema, revision bump via applyRevision)
/// — see the header for the contract.
void DataSet::landWireSchema(const std::string & datasetId, uint64_t rows, const Json::Value & schema)
{
	_schemaRows		= rows;
	_schemaColumns.clear();
	_schemaColumnIndex.clear();

	if (schema.isArray())
		for (const Json::Value & col : schema)
		{
			ColumnInfo info;

			info.name			= col.get("name", "").asString();
			info.displayName	= col.get("display_name", info.name).asString();
			info.description	= col.get("description", "").asString();

			const std::string wireType = col.get("type", "scale").asString();
			info.type =	wireType == "scale"		? columnType::scale
					:	wireType == "ordinal"	? columnType::ordinal
					:							  columnType::nominal;

			info.allInteger		= col.get("all_integer", false).asBool();

			// Constraint-check stats (data-model-design.md §2): non-empty count + distinct count
			// from the lane — they answer every form levels/numeric threshold; nothing in the
			// frontend ever counts distinct values itself.
			if (col.isMember("value_count"))
				info.valueCount = col["value_count"].asUInt64();
			if (col.isMember("distinct_count"))
				info.distinctCount = col["distinct_count"].asUInt64();
			if (col.isMember("numeric_levels"))
				info.numericLevels = col["numeric_levels"].asInt();

			if (col.isMember("levels") && col["levels"].isArray())
				for (const Json::Value & level : col["levels"])
					info.levels.push_back(level.asString());

			_schemaColumnIndex.emplace(info.name, _schemaColumns.size());	// first-wins, same semantics as the old linear scan
			_schemaColumns.push_back(std::move(info));
		}

	// R2 step 4 — THE MIRROR IS GONE (2026-08-31): lane datasets never materialize legacy
	// Columns. The provider chain serves from `schema()` (ColumnModel's NEO adapter;
	// ColumnsModel binds the lane directly), the grid serves from the view lane, and the
	// legacy readers (label editor, computed columns) are gated/inert on lane data — no
	// mirror Column exists to go stale, and the row-sized bug class (§1e3) is extinct by
	// construction. `columnCount()` serves the schema when lane-bound; `column(...)` is
	// nullptr. Legacy datasets keep their Columns untouched.

	setRowCountMetadata(size_t(rows));	// metadata only — never load row data, never materialize legacy vectors

	//The excision, Cut 2: re-register the encoder's names from the fresh schema. setupEncoderPrefix
	//last ran at dbCreate (empty), so without this a lane dataset could never encode a schema name
	//(forms/analyses asking for "V1" would hit "not a columnName"). The prefix itself is
	//id-derived and unchanged; only the name set refreshes.
	_encoder->setCurrentNames(getColumnTypesMap());

	Log::log() << "DataSet: lane schema applied for " << datasetId << " (" << rows << " rows, " << _schemaColumns.size() << " columns)" << std::endl;
	emit schemaChanged();
}

void DataSet::applyRevision(uint64_t revision, uint64_t rows, bool hasRows, const Json::Value & schema, const Json::Value & invalidation)
{
	// §6 ordering rule: per-dataset pushes arrive in revision order; `revision ≤ current`
	// is a stale/replayed push — ignore (idempotent). And only a lane-bound dataset takes
	// revisions at all (legacy imports never see a data_changed).
	if (_laneDatasetId.empty() || revision <= _laneRevision)
		return;

	_laneRevision = revision;

	// v1 whole-buffer semantics (§6 + §11 open item 2): the invalidation descriptor is
	// accepted but not range-applied — schemaChanged restarts the view lane, which drops
	// the whole buffer and refetches at the new revision (sliding mode makes the urgent
	// viewport refetch cheap). Range-aware invalidation replaces the restart later, in the
	// same slot, without touching a single caller.
	if (schema.isArray() && !schema.empty())
		landWireSchema(_laneDatasetId, hasRows ? rows : _schemaRows, schema);	// schema-iff-changed (the lane decides)
	else
	{
		if (hasRows)
			_schemaRows = rows;
		setRowCountMetadata(size_t(_schemaRows));	// metadata only — the lane path never materializes legacy vectors
		Json::StreamWriterBuilder w;
		w["indentation"] = "";
		Log::log() << "DataSet: lane revision " << revision << " for " << _laneDatasetId
				   << " (" << _schemaRows << " rows; invalidation "
				   << (invalidation.isObject() ? Json::writeString(w, invalidation) : std::string("{}"))
				   << ")" << std::endl;
		emit schemaChanged();	// rows/revision refreshed — GridModel restarts the view lane at the new identity
	}
}

const ColumnInfo * DataSet::schemaColumnAt(size_t index) const
{
	return index < _schemaColumns.size() ? &_schemaColumns[index] : nullptr;
}

const ColumnInfo * DataSet::schemaColumn(const std::string & name) const
{
	const int idx = schemaColumnIndex(name);
	return idx < 0 ? nullptr : &_schemaColumns[size_t(idx)];
}

int DataSet::schemaColumnIndex(const std::string & name) const
{
	auto found = _schemaColumnIndex.find(name);
	return found == _schemaColumnIndex.end() ? -1 : int(found->second);
}


