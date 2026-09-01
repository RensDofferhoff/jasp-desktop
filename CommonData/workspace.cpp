#include <QCoreApplication>
#include <QSet>
#include "workspace.h"
#include "qutils.h"
#include "log.h"
#include "undostack.h"
#include "variableinfo.h"

Workspace * Workspace::_singleton = nullptr;

Workspace::Workspace(QObject *parent)
	: DataSetBaseNode{dataSetBaseNodeType::workspace, parent},
	  _varInfo(new VariableInfo(nullptr, this))
{
	assert(!_singleton);
	_singleton = this;

	//The input-filter list (for the shown computed dataset) depends on the set of datasets, their
	//filters, and which dataset is shown; forward all of those so QML's `values` binding stays reactive.
	connect(this, &Workspace::dataSetCreated,			this, &Workspace::inputFilterDropDownListChanged);
	connect(this, &Workspace::dataSetRemoved,			this, &Workspace::inputFilterDropDownListChanged);
	connect(this, &Workspace::shownDataSetChanged,		this, &Workspace::inputFilterDropDownListChanged);
	connect(this, &Workspace::filtersCountChanged,		this, &Workspace::inputFilterDropDownListChanged);
}

Workspace::~Workspace()
{
	assert(_singleton == this);
	
	for(auto & idData : _dataSets)
		unregisterNode(idData.second);
	
	_dataSets.clear();
	
	_singleton = nullptr;
}


// The excision, Cut 3: Workspace::db() died with DatabaseInterface.

QVariant Workspace::data(const QModelIndex &index, int role) const
{
	if(!index.isValid())
		return QVariant();
	
	if(index.row() >= rowCount() || index.column() >= columnCount())
		return QVariant(); // if there is no data then it doesn't matter what role we play
	
	DataSets sets = dataSets();
	DataSet * cur = sets[index.row()];

	switch(role)
	{
	case Qt::DisplayRole:									
	case int(dataPkgRoles::label):							 
	case int(dataPkgRoles::value):							return cur->descriptionQ();
	case int(dataPkgRoles::name):							return cur->name();//tq(cur->db().dataSetName(cur->id()));
	case int(dataPkgRoles::title):							return cur->title();
	case int(dataPkgRoles::description):					return cur->descriptionQ();
	case int(dataPkgRoles::id):								return cur->id();
	case int(dataPkgRoles::computedColumnType):				return int(cur->codeType());
	case int(dataPkgRoles::columnIsComputed):				return cur->isComputed();
	}
	
	return QVariant();
}

void Workspace::setDataMode(bool mode)			
{
	if(_dataMode == mode)
		return;
	
	_dataMode = mode; 
	emit dataModeChanged(_dataMode);
	refresh();
}

void Workspace::setShowRSyntax(bool showRSyntax)				
{ 
	_showRSyntax		= showRSyntax;			
	// was dbUpdate() — the sqlite workspace row died with DatabaseInterface (the excision, Cut 3)
	
	emit showRSyntaxChanged(_showRSyntax);
}

// The excision, Cut 3: Workspace::dbLoad/dbUpdate died with DatabaseInterface — the sqlite
// workspace restore (.jasp persistence) returns in a later NEO era.

void Workspace::dbDelete()
{
	for(auto & idData : _dataSets)
		idData.second->dbDelete();
}

bool Workspace::checkForUpdates(std::function<void(float)> progressCallback)
{
	// The excision, Cut 3: this was the sqlite diff-poll (new/removed dataset rows, showRSyntax
	// reload). Nothing external can mutate the workspace anymore — never anything to update.
	(void) progressCallback;
	return false;
}


DataSet *Workspace::shownDataSet() const
{
	return _shownDataSet;
}

void Workspace::setFormProvider(VariableInfoProvider * provider)
{
	// The excision, Cut 6: the forms' provider is injected (ColumnsModel in the desktop app,
	// DataSetProvider in the engine/test worlds) — the shown Filter no longer serves forms.
	_formProvider = provider;
	if (_varInfo)
		_varInfo->setProvider(provider);
}

void Workspace::setShownDataSet(DataSet *dataSet)
{
	if(_shownDataSet == dataSet)
		return;
	
	assert(dataSet->workspace() == this);
	
	disconnect(_shownDataSet, &DataSet::shownFilterChanged, this, &Workspace::shownFilterChanged);
	
	_shownDataSet = dataSet;
	
	UndoStack::setCurrent(_shownDataSet->undoStack());

	//Column-name encoding/decoding (and computed-column dependency resolution) on the desktop must
	//reflect the shown dataset. Consumers obtain the dataset's own encoder via the provider
	//(provider->columnEncoder()); we only (re)populate its name map here, and never touch the
	//process-global current encoder (that global is only meaningful inside the engine's request context).
	_shownDataSet->encoder().setCurrentNames(_shownDataSet->getColumnTypesMap());
	
	connect(_shownDataSet, &DataSet::shownFilterChanged, this, &Workspace::shownFilterChanged, Qt::UniqueConnection);

	// The excision, Cut 6: the forms are served by the injected provider (ColumnsModel /
	// DataSetProvider), never by the shown Filter's guts.
	_varInfo->setProvider(_formProvider);
			
	emit shownDataSetChanged(_shownDataSet);
	
	refresh();
}

void Workspace::setShownDataSet(int dataSetId)
{
	if(_dataSets.count(dataSetId))
		setShownDataSet(_dataSets.at(dataSetId));
	else
		Log::log() << "setShownDataSet(" << dataSetId << ") can't find the dataSet!" << std::endl;
}

void Workspace::deleteShownDataSet()
{
	if(!_shownDataSet)
		return;

	const int deletedId = _shownDataSet->id();

	//Computed datasets that used a filter of the deleted dataset as their input would otherwise keep a
	//dangling defaultInputFilterId and attempt to run against a dataset that no longer exists. Clear the
	//input and surface an error so the user knows why the computed dataset can no longer be produced
	//instead of silently recomputing against the dataset's own (empty) data.
	for (const auto & idDataSet : _dataSets)
	{
		DataSet * ds = idDataSet.second;
		if (ds != _shownDataSet && ds->isComputed() && ds->defaultInputDataSet() && ds->defaultInputDataSet()->id() == deletedId)
		{
			ds->setDefaultInputFilterId(-1);
			ds->setError("The filter used as input for this computed dataset belonged to a dataset that was removed.");
		}
	}

	_dataSets.erase(deletedId);
	emit dataSetRemoved(deletedId);
	_shownDataSet->dbDelete();
	UndoStack::setCurrent(nullptr);
	delete _shownDataSet;
		
	_shownDataSet = nullptr;
	
	DataSet * newShown = nullptr;
	
	for(auto & idDataSet : _dataSets)
	{
		newShown = idDataSet.second;
		break;
	}
	
	if(newShown)	setShownDataSet(newShown);
	else
	{
		_varInfo->setProvider(nullptr); //No dataset left: don't leave the provider pointing at a destroyed filter
		refresh();
	}
}

void Workspace::showFilter(int id)
{
	Filter * f = filterById(id);
	
	if(f)
	{
		setShownDataSet(f->data());
		f->data()->showFilter(f);
		// The excision, Cut 6: the provider is the injected one (ColumnsModel/DataSetProvider),
		// not the shown filter — repointing at f died with Filter's provider role.
		refresh();
	}
}

void Workspace::onShownFilterChanged(DataSet *dataSet)
{
	setShownDataSet(dataSet);
	emit shownFilterChanged();
}

void Workspace::refreshAllCompCols(Filter *f)
{
	assert(f);
	// The excision, Cut 5: computed-Column invalidation died with Column (they return as
	// derivations). Kept as the signal's relay target (Computed datasets have their own path).
}

void Workspace::setShownDataSet(QString name)
{
	for(auto & idData : _dataSets)
		if(idData.second->name() == name)
		{
			setShownDataSet(idData.second);
			return;
		}
}

DataSets Workspace::dataSets() const
{
	DataSets out;
	
	for(auto & idData : _dataSets)
		out.push_back(idData.second);
	
	return out;
}

DataSet *Workspace::dataSetById(int id) const
{
	if(_dataSets.count(id))
		return _dataSets.at(id);
	
	return nullptr;
}

DataSet *Workspace::dataSetByName(const std::string & name) const
{
	for(auto & idData : _dataSets)
		if(idData.second->name().toStdString() == name)
			return idData.second;

	return nullptr;
}

DataSet *Workspace::dataSetByLaneId(const std::string & laneId) const
{
	if(laneId.empty())
		return nullptr;

	for(auto & idData : _dataSets)
		if(idData.second->datasetId() == laneId)
			return idData.second;

	return nullptr;
}

DataSet *Workspace::applyLaneRevision(const std::string & laneId, uint64_t revision, uint64_t rows, bool hasRows, const Json::Value & schema, const Json::Value & invalidation)
{
	DataSet * ds = dataSetByLaneId(laneId);
	if(!ds)
	{
		// A push for a dataset we do not hold (closed tab, or its open never landed): nothing
		// to invalidate — drop it, never crash on it (§6: the broadcast goes to every holder).
		Log::log() << "Workspace: data_changed for unknown dataset '" << laneId << "' ignored." << std::endl;
		return nullptr;
	}

	ds->applyRevision(revision, rows, hasRows, schema, invalidation);
	return ds;
}

QString Workspace::makeDataSetTitleUnique(const QString & title, DataSet * exclude) const
{
	QSet<QString> takenTitles;
	for(const auto & idData : _dataSets)
		if(idData.second != exclude)
			takenTitles.insert(idData.second->title());

	if(!takenTitles.contains(title))
		return title;

	int suffix = 2;
	QString candidate;
	do
		candidate = title + " (" + QString::number(suffix++) + ")";
	while(takenTitles.contains(candidate));

	return candidate;
}

Filter *Workspace::filterById(int id) const
{
	for(auto & idDataSet : _dataSets)
		if(idDataSet.second->filter(id))
			return idDataSet.second->filter(id);
	return nullptr;
}

Filter *Workspace::shownFilter() const
{
	return shownDataSet() ? shownDataSet()->shownFilter()	: nullptr;
}

void Workspace::setShownFilter(Filter *newShownFilter)
{
	newShownFilter->data()->showFilter(newShownFilter);
	setShownDataSet(newShownFilter->data());
}


DataSet * Workspace::createDataSet()
{
	bool shownDataSetExistsAndIsEmpty = 
			_shownDataSet && 
			(_shownDataSet->columnCount() == 0 // Simple case
			|| (_shownDataSet->columnCount() == 1 && _shownDataSet->rowCount() == 1 && _shownDataSet->data(_shownDataSet->index(0, 0)) == QVariant())); //Single empty cell
	
	if(shownDataSetExistsAndIsEmpty)
		return _shownDataSet;

	DataSet * newSet = new DataSet(this);

	if(!_shownDataSet)
		setShownDataSet(newSet);

	_dataSets[newSet->id()] = newSet;

	emit dataSetCreated(newSet->id());
	emit filtersCountChanged(); //Triggers filterDropDownListChanged in filtermodel

	return newSet;
}

	// The excision, Cut 5: createComputedColumn died with Column — computed columns return
	// as derivations (ChangeKind::derived) in a later era.

DataSet *Workspace::createComputedDataSet(const std::string &name, int defaultInputFilterId, computedColumnType desiredType)
{
	DataSet * newSet = createDataSet();

	if(!newSet)
		return nullptr;

	newSet->setTitle(tq(name));
	newSet->setCodeType(desiredType);
	newSet->setDefaultInputFilterId(defaultInputFilterId);
	newSet->invalidate();

	setShownDataSet(newSet);

	return newSet;
}

QStringList Workspace::dataSetNames() const
{
	QStringList names;

	for(const auto & idData : _dataSets)
		names.push_back(idData.second->name());

	return names;
}

QVariantList Workspace::inputFilterDropDownList() const
{
	typedef QMap<QString, QVariant> localMap;

	//The filters available as *input* for the currently-shown computed dataset: every dataset's
	//filters except the shown dataset's own. A computed dataset must not read from its own output,
	//and setDefaultInputFilterId would refuse it as a loop anyway, so hide it here too.
	const int excludeDataSetId = _shownDataSet ? _shownDataSet->id() : -1;

	QVariantList out;

	for(const auto & idData : _dataSets)
	{
		DataSet * dataSet = idData.second;

		if(dataSet->id() == excludeDataSetId)
			continue;

		//out.append(localMap{std::make_pair("value", tq("-")), std::make_pair("label", dataSet->title() + ":")});

		if(dataSet->defaultFilter())
			out.append(localMap{std::make_pair("value", tq(std::to_string(dataSet->defaultFilter()->id()))), std::make_pair("label", dataSet->title() + " - " + dataSet->defaultFilter()->title())});

		for(const Filter * f : dataSet->filters())
			if(f != dataSet->defaultFilter())
				out.append(localMap{std::make_pair("value", tq(std::to_string(f->id()))), std::make_pair("label", dataSet->title() + " - " + f->title())});
	}

	return out;
}

bool Workspace::wouldCreateComputedDataSetLoop(DataSet * me, DataSet * target) const
{
	std::set<int> visited;
	DataSet * cur = target;
	while (cur)
	{
		if (cur == me)
			return true;

		if (!visited.insert(cur->id()).second)
			return false; //Reached a node we have seen; not a loop involving me.

		if (!cur->isComputed() || !cur->defaultInputDataSet())
			return false;

		cur = cur->defaultInputDataSet();
	}

	return false;
}

bool Workspace::computedDataSetsHaveLoop(std::string & errorMessage) const
{
	std::set<int> globalVisited;

	for (const auto & idData : _dataSets)
	{
		DataSet * ds = idData.second;
		if (!ds->isComputed() || globalVisited.count(ds->id()))
			continue;

		std::set<int> chain;
		DataSet * cur = ds;

		while (cur && cur->isComputed())
		{
			if (!chain.insert(cur->id()).second)
			{
				errorMessage = "A loop was found between your computed datasets and their input datasets. Change one of the input selections to break the circle.";
				return true;
			}

			if (!cur->defaultInputDataSet())
				break;

			cur = cur->defaultInputDataSet();
		}

		globalVisited.insert(chain.begin(), chain.end());
	}

	return false;
}

void Workspace::setDataSetComputed(const QString & name, bool computed)
{
	DataSet * ds = dataSetByName(fq(name));
	if(!ds || computed == ds->isComputed())
		return;

	if(ds != shownDataSet())
		setShownDataSet(ds);

	if(computed)
	{
		ds->setCodeType(computedColumnType::rCode);

		if(!ds->defaultInputDataSet())
		{
			//Pick the first other dataset whose default filter does not close a loop (e.g. another
			//computed dataset that (in)directly depends on this one must not be chosen, or we would
			//create A <- B <- A).
			DataSet * candidate = nullptr;
			for(const auto & idData : _dataSets)
				if(idData.second != ds && !wouldCreateComputedDataSetLoop(ds, idData.second))
				{
					candidate = idData.second;
					break;
				}

			if(candidate && candidate->defaultFilter())
				ds->setDefaultInputFilterId(candidate->defaultFilter()->id());
		}

		if(ds->defaultInputDataSet() && wouldCreateComputedDataSetLoop(ds, ds->defaultInputDataSet()))
			ds->setError("The filter chosen as input for this computed dataset would create a loop between the computed datasets.");
	}
	else
		ds->setCodeType(computedColumnType::notComputed);

	DataSets sets = dataSets();

	for(int i = 0; i < sets.size(); ++i)
		if(sets[i] == ds)
		{
			emit dataChanged(index(i, 0), index(i, 0), { int(dataPkgRoles::columnIsComputed), int(dataPkgRoles::computedColumnType), int(dataPkgRoles::id) });
			break;
		}
}

void Workspace::refresh()
{
	//Skip nested/re-entrant refreshes entirely (e.g. a dataset refresh emitting a signal that
	//triggers Workspace::refresh again): doing beginResetModel/endResetModel while a reset is
	//already in progress is undefined behaviour in Qt.
	//RAII guard (unlike a plain counter) so the flag is cleared even if a signal handler throws.
	struct RefreshGuard
	{
		bool &	_inRefresh;
		explicit RefreshGuard(bool & inRefresh) : _inRefresh(inRefresh) { _inRefresh = true; }
		~RefreshGuard()                             { _inRefresh = false; }
	};

	//instance flag (see _inRefresh in workspace.h), not static, so it cannot suppress refreshes
	//across separate Workspace instances.
	if (!_inRefresh)
	{

		RefreshGuard guard(_inRefresh);
		beginResetModel();
	
		for(auto & idData : _dataSets)
			idData.second->refresh();
	
		emit dataModeChanged(dataMode());
		emit showRSyntaxChanged(showRSyntax());
		endResetModel();
	}

	//Emit the "shown" signals only after the reset is complete: these connect into QML/other models
	//that may re-query the Workspace model, which is not allowed while a reset is still active.
	emit shownDataSetChanged(shownDataSet());
	emit shownFilterChanged();
}


// The excision, Cut 5: initializeComputedColumns walked the legacy Columns for computed
// dependencies — gone with Column.

void Workspace::initializeComputedDatasets()
{
	for(auto & idDataSet : _dataSets)
		if(idDataSet.second->isComputed() && idDataSet.second->iShouldBeSentAgain())
			idDataSet.second->tryAndRunComputedDataset();
}

void Workspace::computedDataSetSucceeded(int dataSetId, QString warning, bool dataChanged)
{
	DataSet * dataSet = dataSetById(dataSetId);

	if(!dataSet)
		return;

	dataSet->checkForUpdates();
	dataSet->setError(warning.isEmpty() ? std::string() : fq(warning));

	//A failed computation leaves the dataset invalidated so it stays marked as needing a (re)run;
	//only a successful computation validates it and lets the datasets depending on it proceed.
	if(!warning.isEmpty())
		return;

	//Never cascade into a cycle (A <- B <- A): if the computed-dataset graph has a loop, do not keep
	//recomputing; surface the error and leave the datasets invalidated so the user fixes the inputs.
	std::string loopError;
	if (computedDataSetsHaveLoop(loopError))
	{
		for (const auto & idData : _dataSets)
			if (idData.second->isComputed() && idData.second->invalidated())
				idData.second->setError(loopError);
		return;
	}

	dataSet->validate();
	dataSet->checkForDependentDatasetsToBeSent();
}

// The excision, Cut 5: the legacy computed-column bookkeeping (dependency tracking + the
// engine's computedColumnSucceeded callback) died with Column — computed columns return as
// derivations (ChangeKind::derived) with their own producer on the rail.
