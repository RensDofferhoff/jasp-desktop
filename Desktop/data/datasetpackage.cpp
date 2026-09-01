//
// Copyright (C) 2018 University of Amsterdam
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 2 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//

#include "datasetpackage.h"
#include "log.h"
#include "qutils.h"
#include <QThread>
#include "timers.h"
#include "utils.h"
#include "columnencoder.h"
#include "utilities/appdirs.h"
#include "gui/preferencesmodel.h"
#include "utilities/messageforwarder.h"
#include "databaseconnectioninfo.h"
#include "filtermodel.h"
#include "utilities/settings.h"
#include <ranges>
#include "variableinfo.h"
#include "fileevent.h"
#include "jaspclient/jaspclient.h"		///< NEO: data_open submission lane


DataSetPackage * DataSetPackage::_singleton = nullptr;

DataSetPackage::DataSetPackage(QObject * parent) : QObject(parent)
{
	if(_singleton) throw std::runtime_error("DataSetPackage can be constructed only once!");
	_singleton = this;
	//NEO: true init is done in MainWindow after the client is up (was: setEngineSync)
	// The excision, Cut 3: the package no longer owns a DatabaseInterface (was: _db = new ...).

	createWorkspace();
	
	connect(this, &DataSetPackage::isModifiedChanged,					this, &DataSetPackage::windowTitleChanged);
	connect(this, &DataSetPackage::loadedChanged,						this, &DataSetPackage::windowTitleChanged);
	connect(this, &DataSetPackage::currentFileChanged,					this, &DataSetPackage::windowTitleChanged);
	connect(this, &DataSetPackage::folderChanged,						this, &DataSetPackage::windowTitleChanged);
	connect(this, &DataSetPackage::isModifiedAfterAutoSaveChanged,		this, &DataSetPackage::windowTitleChanged);
	connect(this, &DataSetPackage::currentFileChanged,					this, &DataSetPackage::nameChanged);
	connect(this, &DataSetPackage::dataModeChanged,						this, &DataSetPackage::onDataModeChanged);
	
	connect(PreferencesModel::prefs(), &PreferencesModel::autoSaveAtAllChanged,			this, &DataSetPackage::handleAutoSavePrefChange);
	connect(PreferencesModel::prefs(), &PreferencesModel::autoSaveIntervalSecChanged,	this, &DataSetPackage::handleAutoSavePrefChange);

		connect(&_autoSaveTimer,			&QTimer::timeout, this, &DataSetPackage::handleAutoSave);
	
	_autoSaveTimer			.setSingleShot(false);
	handleAutoSavePrefChange();

	// Multi-dataset fold: Workspace is the one dataset truth. The shown dataset's lane id
	// IS datasetId() — relay shownDataSetChanged as datasetIdChanged for legacy listeners.
	if (_workspace)
		connect(_workspace, &Workspace::shownDataSetChanged, this, [this](DataSet *) { emit datasetIdChanged(); });
}

DataSetPackage::~DataSetPackage() 
{ 
	_singleton = nullptr; 
}


void DataSetPackage::createWorkspace()
{
	assert(!_workspace);
	
	_workspace = new Workspace(this);
	
	_workspace->setShowRSyntax(PreferencesModel::prefs() ? PreferencesModel::prefs()->showRSyntaxInResults() : false);
	
	connectWorkspace();
	
	emit workspaceChanged();
}

DataSet * DataSetPackage::createDataSet()
{
	JASPTIMER_SCOPE(DataSetPackage::createDataSet);
	
	//The assumption here is that a new DataSet is needed. But not that anything else needs to be destroyed.
	
	if(!_workspace)
		createWorkspace();
	
	DataSet * dataSet = workspace()->createDataSet();
	
	//A brand new DataSet should start out with the configured default workspace empty values
	//(this also covers unittests, where there is no PreferencesModel).
	setDefaultWorkspaceEmptyValues();
		
	return dataSet;
}

// The excision, Cut 3: DataSetPackage::loadWorkspace (the .jasp sqlite restore) died with
// DatabaseInterface — its only callers were the JASP-import paths removed in Cut 2. The
// .jasp restore returns in a later NEO era.

void DataSetPackage::deleteWorkspace(bool dbDeletePlease)
{
	JASPTIMER_SCOPE(DataSetPackage::deleteWorkspace);
	
	if(dbDeletePlease)
		dbDelete();
	delete _workspace;
	_workspace = nullptr;

	//Always notify so QML can re-point its 'workspace' context (to null) instead of keeping a
	//dangling pointer to the just-deleted Workspace.
	emit shownDataSetChanged(nullptr);
	emit workspaceChanged();
}

void DataSetPackage::connectWorkspace()
{
	if(!workspace())
		return;
	
	Workspace		::connect(workspace(),	&Workspace::showWarning,						this,			&DataSetPackage::showWarning						);
	Workspace		::connect(workspace(),	&Workspace::showAnalysis,						this,			&DataSetPackage::showAnalysis						);
	Workspace		::connect(workspace(),	&Workspace::datasetChanged,						this,			&DataSetPackage::datasetChanged						);
	Workspace		::connect(workspace(),	&Workspace::somethingModified,					this,			&DataSetPackage::setModifiedFileMenu				);
	Workspace		::connect(workspace(),	&Workspace::dataModeChanged,					this,			&DataSetPackage::dataModeChanged					);
	Workspace		::connect(workspace(),	&Workspace::sendFilter,							this,			&DataSetPackage::sendFilter							);
	Workspace		::connect(workspace(),	&Workspace::sendFilterByName,					this,			&DataSetPackage::sendFilterByName					);
	Workspace		::connect(workspace(),	&Workspace::filtersCountChanged,				this,			&DataSetPackage::filtersCountChanged				);
	Workspace		::connect(workspace(),	&Workspace::shownFilterChanged,					this,			&DataSetPackage::shownFilterChanged					);
	Workspace		::connect(workspace(),	&Workspace::refreshAllAnalyses,					this,			&DataSetPackage::refreshAllAnalyses					);
	Workspace		::connect(workspace(),	&Workspace::shownDataSetChanged,				this,			&DataSetPackage::shownDataSetChanged				);	
	Workspace		::connect(workspace(),	&Workspace::dataSetCreated,						this,			&DataSetPackage::dataSetCreated					);	
	Workspace		::connect(workspace(),	&Workspace::dataSetRemoved,						this,			&DataSetPackage::dataSetRemoved					);	
	//A manual edit by the user (in the data grid / paste) means external-file syncing should be disabled.
	Workspace		::connect(workspace(),	&Workspace::manualEditMade,						this,			[this]{ setManualEdits(true); }					);
	Workspace		::connect(workspace(),	&Workspace::runComputedDataSet, this, &DataSetPackage::runComputedDataSet						);
	Workspace		::connect(workspace(),	&Workspace::emptyValuesChanged,			this,			&DataSetPackage::workspaceEmptyValuesChanged		);

	DataSetPackage	::connect(this,			&DataSetPackage::filterByNameDone,				workspace(),	&Workspace::filterByNameDone						);
	
	
	emit shownDataSetChanged(nullptr);
	emit shownFilterChanged();
}


Filter * DataSetPackage::filter()
{
    return pkg()->workspace() && pkg()->workspace()->shownDataSet() ? pkg()->workspace()->shownDataSet()->shownFilter() : nullptr;
}

void DataSetPackage::reset(bool newDataSet)
{
	emit chooseColumn(-1); //Unselect any column in ColumnModel
	
	deleteWorkspace();

	if(newDataSet)	
		createDataSet();
	
	_archiveVersion				= Version();
	_jaspVersion				= Version();
	_analysesHTML				= QString();
	_analysesData				= Json::arrayValue;
	_warningMessage				= std::string();
	_hasAnalysesWithoutData		= false;
	_analysesHTMLReady			= false;
	_isJaspFile					= false;

	setLoaded(false);
	setModified(false);
	setCurrentFile("");
}

///This function assumes there should afterwards be only 1 DataSet!
void DataSetPackage::generateEmptyData()
{
	bool wasAlreadyLoaded = isLoaded();

	if(workspace())
		deleteWorkspace();
	createWorkspace();
	
	DataSet * newSet = dataSet() ? dataSet() : createDataSet();
	
	// The excision, Cut 5: the empty dataset used to materialize one legacy Column via
	// initFromLookups — play a minimal LANE schema instead ("New Data" = a 1×1 scale sheet).
	Json::Value schema(Json::arrayValue);
	Json::Value col(Json::objectValue);
	col["name"]				= "New Column";
	col["display_name"]	= "New Column";
	col["type"]				= "scale";
	col["value_count"]		= Json::UInt64(1);
	col["distinct_count"]	= Json::UInt64(0);
	schema.append(col);
	newSet->applySchema("new-data", 1, schema, "");
	
	setModified(false);
	
	if(!wasAlreadyLoaded)
	{
		emit newDataLoaded();
	}
	
	newSet->resetAllFilters();
	newSet->setDataFileSynch(false);
	
	if(workspace()->shownDataSet() != newSet)
		workspace()->setShownDataSet(newSet);
	else
		workspace()->refresh();
}

void DataSetPackage::onDataModeChanged(bool dataMode)
{
	if(workspace())
		workspace()->setDataMode(dataMode);
}

void DataSetPackage::setModified(bool value)
{
	if ((!value || _isLoaded || _hasAnalysesWithoutData) && value != _isModified)
	{
		_isModified = value;
		emit isModifiedChanged();
	}
	
	setModifiedAfterAutoSave(_isModified);
}

void DataSetPackage::setModifiedAfterAutoSave(bool value)
{
	if (value != _isModifiedAfterAutoSave)
	{
		_isModifiedAfterAutoSave = value;
		emit isModifiedAfterAutoSaveChanged();
	}
}


void DataSetPackage::handleAutoSave()
{
	if(_isModifiedAfterAutoSave)				
		emit makeAnAutoSave();
	
	else if(FileEvent::autoSaveExists() && _isModified)
			Utils::touch(fq(FileEvent::pathTmp()));
}


void DataSetPackage::setLoaded(bool loaded)
{
	if(loaded == _isLoaded)
		return;

	_isLoaded						= loaded;

	emit loadedChanged();
}

QString DataSetPackage::description() const
{
	return tq(dataSet() ? dataSet()->description() : "");
}

void DataSetPackage::setDescription(const QString &description)
{
	if (!dataSet()) return;
	
	dataSet()->setDescription(fq(description));

	emit descriptionChanged();
}

void DataSetPackage::prepareForLanguageChange()
{
	_waitingForLanguageChange = true; //Dont accept changes while the interface changes
}

void DataSetPackage::languageChangeDone()
{
	_waitingForLanguageChange = false; //Dont accept changes while the interface changes

	if(dataSet())
		dataSet()->refresh();
}

void DataSetPackage::handleAutoSavePrefChange()
{
	_autoSaveTimer.setInterval(1000 * (PreferencesModel::prefs() ? PreferencesModel::prefs()->autoSaveIntervalSec() : 1));
	
	if(PreferencesModel::prefs() && _autoSaveTimer.isActive() != PreferencesModel::prefs()->autoSaveAtAll())
	{
		if(!PreferencesModel::prefs()->autoSaveAtAll())		
			_autoSaveTimer.stop();
		else
			_autoSaveTimer.start();
	}
}


void DataSetPackage::refreshColumn(QString columnName)
{
	// The excision, Cut 5: this refreshed a legacy Column's in-memory state — the schema
	// changed signal chain covers it now.
	Q_UNUSED(columnName);
	refresh(); //Hopefully trigger sortfilterproxymodel model reconstruction
}


void DataSetPackage::columnWasOverwritten(const std::string & columnName, const std::string &)
{
	// The excision, Cut 5: this re-aired a legacy Column's change — schemaChanged covers it.
	Q_UNUSED(columnName);
}


void DataSetPackage::refresh()
{
	if(!dataSet())
		return;
	
	dataSet()->refresh();
}




void DataSetPackage::stopEngines()
{
	// NEO: the legacy engine scheduler is gone; the orchestrator manages its own lanes.
}

void DataSetPackage::restartEngines()
{
	// NEO: the legacy engine scheduler is gone; the orchestrator manages its own lanes.
}



void DataSetPackage::dbDelete()
{
	JASPTIMER_SCOPE(DataSetPackage::dbDelete);

	if(!workspace())
		return;

	//NOTE (deliberate semantics): this is a FULL teardown (New / close file), so it permanently purges
	//*every* dataset from SQLite, not only the shown one. Callers must pair this with a full
	//Analyses/UI reset so nothing keeps a reference to the purged datasets. Single-dataset deletion is
	//a different operation and does NOT go through here (see Workspace::deleteShownDataSet).
	DataSets sets = workspace()->dataSets();
	for (DataSet * ds : sets)
		if (ds && ds->id() != -1)
			ds->dbDelete();
}

int DataSetPackage::thresholdScale()
{
	//In unittests there is no PreferencesModel, so fall back to the configured default (10) instead of a hardcoded value.
	return PreferencesModel::prefs() ? PreferencesModel::prefs()->thresholdScale() : Settings::value(Settings::THRESHOLD_SCALE).toInt();
}

int DataSetPackage::orderByValueByDefault()
{
	return PreferencesModel::prefs() ? int(PreferencesModel::prefs()->orderByValueByDefault()) : true;
}

void DataSetPackage::resetVariableTypes()
{
	// The excision, Cut 5: type re-guessing scanned legacy Column values — the lane owns
	// typing; returns with backend sync (P14 territory).
}

bool DataSetPackage::workspaceShowRSyntax() const
{
	return workspace() ? workspace()->showRSyntax() : (PreferencesModel::prefs() ? PreferencesModel::prefs()->showRSyntaxInResults() : false);
}


void DataSetPackage::setDataSetEmptyValues(const stringset &emptyValues, bool reset)
{
	if (!workspace()) 
		return;
	
	
	for(DataSet * dataSet : workspace()->dataSets())
		dataSet->setEmptyValuesFromStrings(emptyValues);
	
	if(reset)	
		refresh();
	
	emit workspaceEmptyValuesChanged();
}

void DataSetPackage::setDefaultWorkspaceEmptyValues()
{
	stringvec prefs;

	if (PreferencesModel::prefs())
		prefs = fq(PreferencesModel::prefs()->emptyValues());
	else if (Settings::value(Settings::EMPTY_VALUES_LIST).isValid())
	{
		// In unittests there is no PreferencesModel, but we still want to apply the configured
		// default empty values (Settings::value(EMPTY_VALUES_LIST) returns them in test mode too).
		QStringList items = Settings::value(Settings::EMPTY_VALUES_LIST).toString().split("|");
		std::set<QString> ordered(items.begin(), items.end());
		prefs = fq(QStringList(ordered.begin(), ordered.end()));
	}

	setDataSetEmptyValues(stringset(prefs.begin(), prefs.end()));
}

void DataSetPackage::setWorkspaceShowRSyntax(bool show)
{
	if (!workspace() || workspace()->showRSyntax() == show) 
		return;

	workspace()->setShowRSyntax(show);

	setModified(true);
}


void DataSetPackage::setCurrentFile(QString currentFile)
{
	if (_currentFile == currentFile)
		return;

	_currentFile = currentFile;
	emit currentFileChanged();

	QFileInfo	file(_currentFile);
	QUrl		url(_currentFile);

#ifdef _WIN32
	setFolder(file.exists() ? file.absolutePath().replace('/', '\\')	: url.isValid() ? "OSF" : "");
#else
	setFolder(file.exists() ? file.absolutePath()						: url.isValid() ? "OSF" : "");
#endif
}

void DataSetPackage::setFolder(QString folder)
{
	//Remove the last part if it is the name of the file regardless of extension
	QString _name	= name();
	int		i		= _name.size();
	for(; i < folder.size(); i++)
		if(folder.right(i).startsWith(_name))
		{
			folder = folder.left(folder.size() - i);
			break;
		}
#ifdef _WIN32
		else if(folder.right(i).contains('\\'))	break;
#else
		else if(folder.right(i).contains('/'))	break;
#endif

	if (_folder == folder)
		return;

	_folder = folder;
	emit folderChanged();
}

QString DataSetPackage::name() const
{
	QFileInfo	file(_currentFile);

	if(file.completeBaseName() != "")
		return file.completeBaseName();

	return "JASP";
}

bool DataSetPackage::dataMode() const
{
	return workspace() && workspace()->dataMode();
}

QString DataSetPackage::windowTitle() const
{
	QString name	= DataSetPackage::name(),
			folder	= DataSetPackage::folder();
	
#ifdef _WIN32
	if(folder.startsWith(AppDirs::examples().replace('/', '\\')))
#else
	if(folder.startsWith(AppDirs::examples()))
#endif
		folder = "";

	folder = folder == "" ? "" : "      (" + folder + ")";

	return name + (isModified() ? isModifiedAfterAutoSave() ? "*" : "* (autosaved)"  : "") + folder;
}


bool DataSetPackage::currentJaspFileIsNonSaveable() const
{
	return filePathIsNonSaveable(currentFile());
}

bool DataSetPackage::filePathIsNonSaveable(const QString & path) const
{
	QFileInfo fileDir(path);

	return fileDir.dir().absolutePath().startsWith(AppDirs::examples()) || fileDir.dir() == QDir(AppDirs::autoSaveDir());
}

void DataSetPackage::setAnalysesData(const Json::Value &analysesData)
{
	QString		previousASF					= analysesData.type() != Json::objectValue ? "" : tq(analysesData.get("autoSaveFileName", "").asString());
				_analysesData				= analysesData;
	QFileInfo	dataFile					( tq(dataSet() ? dataSet()->dataFilePath() : "") ),
				curFileI					( currentFile() );
	QString		dataFileName				= dataFile.fileName(),
				curFile						= currentFile(),
				autoSaveString				= curFile != "JASP" ? tr("%1 autosaved").arg(curFileI.fileName()) + "<br>" + tr("Full path: %1").arg("<code>"+curFileI.absoluteFilePath()+"</code>") : dataFileName == "" ? tr("Unsaved workspace") : tr("Unsaved workspace of datafile %1").arg(dataFileName);

	_analysesData["autoSaveDescription"]	= fq(autoSaveString);
	_analysesData["autoSaveFileName"]		= fq(curFileI.exists() ? curFileI.fileName() : previousASF != "" ? previousASF : curFile != "" ? curFile : tr("Autosave"));
}


QString DataSetPackage::autoSavedFileName() const
{
	return tq(_analysesData.get("autoSaveFileName", fq(currentFile())).asString());
}

// This function can be called from a different thread then where the underlying value for isReady() is set, but I don't think a mutex or whatever is necessary here. What could go wrong with checking a boolean?
// Also this was already the case, so I'm not making things worse here...
void DataSetPackage::waitForExportResultsReady() 
{ 
	int maxSleepTime	= 10000,
		sleepTime		= 100,
		delay			= 0;
	
	while (!isReady())
	{
		if (delay > maxSleepTime)
			break;
		
		Utils::sleep(sleepTime);
		delay += sleepTime;
	}
	
	if(!isReady())
		Log::log() << "Results were not exported properly!" << std::endl; //Should we maybe create a dummy result that explains something went wrong with the upload? Should we abort saving? What is going on?
}


void DataSetPackage::checkDataSetForUpdates()
{
	if(!_workspace)
		return;

	_workspace->checkForUpdates();
}

bool DataSetPackage::manualEdits() const
{
	return _manualEdits;
}

void DataSetPackage::setManualEdits(bool newManualEdits)
{
	if (_manualEdits == newManualEdits)
		return;

	_manualEdits = newManualEdits;

	//Editing the data by hand means the external data file no longer reflects the workspace: disable
	//external synching for the (shown) dataset so the next file change doesn't silently revert the
	//user's edits. This is per-dataset now (the shown dataset owns its own DataSetSyncer).
	if(_manualEdits && dataSet())
		dataSet()->setDataFileSynch(false);

	emit manualEditsChanged();
}

// ————— NEO data model (data-model-design.md §3.2) —————
// These five carry the orchestrator-lane surface of the old forwarding API until the fold
// commit moves them onto Workspace/DataSet proper (merge-multidataset.md).

std::string DataSetPackage::datasetId() const
{
	// Multi-dataset fold: identity lives on the DataSet — the shown dataset's orchestrator id.
	return dataSet() ? dataSet()->datasetId() : std::string();
}

QVariant DataSetPackage::getColumnTypesWithIcons() const
{
	static QVariantList ColumnTypeAndIcons;

	if(ColumnTypeAndIcons.size() == 0)
	{
		ColumnTypeAndIcons.push_back("");
		ColumnTypeAndIcons.push_back("variable-scale.svg");
		ColumnTypeAndIcons.push_back("variable-ordinal.svg");
		ColumnTypeAndIcons.push_back("variable-nominal.svg");
	}

	return QVariant(ColumnTypeAndIcons);
}

void DataSetPackage::setDataFilePath(std::string filePath, long timestamp)
{
	DataSet*	dset = dataSet();
	if(!dset || (dset->dataFilePath() == filePath && timestamp == dset->dataFileTimestamp()))
		return;

	if (timestamp == 0 && !filePath.empty())
	{
		QFileInfo fileInfo(tq(filePath));
		timestamp = fileInfo.isFile() ? fileInfo.lastModified().toSecsSinceEpoch() : 0;
	}

	dset->setDataFile(filePath, timestamp);
	if (tq(filePath).startsWith(AppDirs::examples()))
		setFileReadOnly(true);

	setModified(true);
	emit synchingExternallyChanged(synchingExternally());
}

void DataSetPackage::neoOpenDataset(std::string filePath)
{
	// NEO data plane (dataset-manager-design §5.1): opening a dataset is just a WORK — the
	// orchestrator mints the dataset_id, assigns the cache path, routes the conversion to a
	// lane, and answers with the normal terminal result — a kind:"data" result carrying the
	// dataset_id on its typed Data payload (§19.2). Submitted through JaspClient::submit like
	// any analysis work — no special message, no special client path. Main-thread only
	// (MainWindow::dataSetIOCompleted calls this once a file open succeeds); the CSV bytes are
	// read by the data-runner PROCESS, not here — nothing in this call touches the file or a
	// loader thread.
	const QString qPath = tq(filePath);
	// CSV family: the lane sniffs the delimiter (, ; \t |), so .txt/.tsv ride the same op.
	// Must match AsyncLoader::loadPackage's laneOwned set — these formats no longer have a
	// frontend import.
	const bool csvFamily = qPath.endsWith(".csv", Qt::CaseInsensitive)
					  || qPath.endsWith(".txt", Qt::CaseInsensitive)
				  || qPath.endsWith(".tsv", Qt::CaseInsensitive);
	if (!csvFamily)
		return;
	if (!JaspClient::client())
		return;

	// NEO wire shape: op data_open; cache_path is orchestrator-assigned at dispatch (the
	// lane writes the Arrow there). ingest carries the JASP "threshold for scale"
	// preference; the other knobs take orchestrator defaults for now.
	Json::Value ingest(Json::objectValue);
	ingest["threshold"] = PreferencesModel::prefs()->thresholdScale();

	Json::Value payload(Json::objectValue);
	payload["op"]			= "data_open";
	payload["source"]		= filePath;
	payload["cache_path"]	= "";	// assigned by the orchestrator at dispatch
	payload["format"]		= "csv";
	payload["ingest"]		= ingest;

	const std::string workId = "data-open-" + std::to_string(_nextDataOpen++);

	Json::Value work(Json::objectValue);
	work["v"]			= 1;
	work["type"]		= "work";
	work["id"]			= workId;
	work["work_id"]		= workId;
	work["revision"]	= 0;
	work["dataset_ids"]	= Json::Value(Json::arrayValue);
	work["kind"]		= "data";
	work["payload"]		= payload;

	Log::log() << "NEO dataset_open work " << workId << ": " << filePath << std::endl;
	JaspClient::client()->submit(work, [this, filePath](const JaspClient::Result & result)
	{
		if (result.status == "running")
			return;	// park marker: the open is parked while the lane boots — not a failure
		if (result.status != "complete")
		{
			Log::log() << "NEO dataset_open failed (" << result.status << "): " << result.message << std::endl;
			return;
		}
		Log::log() << "NEO dataset ready: " << result.datasetId
				   << " (" << result.rows << " rows)" << std::endl;
		// Multi-dataset fold: the typed kind:"data" payload (dataset_id, rows, schema)
		// lands on the SHOWN DataSet itself — identity + wire schema live there now
		// (data-model-design.md §3.2); applySchema also mirrors the metadata into the
		// legacy columns so the per-dataset provider chain serves it.
		if (DataSet * ds = dataSet())
			ds->applySchema(result.datasetId, result.rows, result.schema, filePath);
		emit datasetIdChanged();
	});
}

void DataSetPackage::setDatabaseJson(const Json::Value &dbInfo)
{
	_database						= dbInfo;
	Log::log() << "DataSetPackage::setDatabaseJson got:" << dbInfo << std::endl;

	if (DataSet * dset = dataSet())
		dset->setDatabaseJson(_database.toStyledString());
}

