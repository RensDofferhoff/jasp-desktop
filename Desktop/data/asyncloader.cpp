//
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

#include "asyncloader.h"


#include <fstream>
#include <QTimer>
#include <QFileInfo>
#include <QThread>

#include "qutils.h"
#include "utils.h"
#include "osf/onlinedatamanager.h"
#include "log.h"
#include "utilenums.h"
#include "appinfo.h"
#include "gui/preferencesmodel.h"
#include "data/datasetpackage.h"

using namespace std;

namespace {
	/// DataSetLoader died with the importers (the excision, Cut 2); this tiny helper stays.
	string getExtension(const string & locator, const string & fallback)
	{
		string ext = std::filesystem::path(locator).extension().generic_string();
		return ext.length() ? ext : fallback;
	}
}

LoaderException::LoaderException(const std::string & _problemDescription, bool _cancelled)
	: std::runtime_error(_problemDescription), cancelled(_cancelled)
{}

AsyncLoader::AsyncLoader(QObject *parent) :
	QObject(parent)
{
	connect(this, &AsyncLoader::beginLoad, this, &AsyncLoader::loadTask, Qt::QueuedConnection);
}

void AsyncLoader::io(FileEvent *event)
{
	switch (event->operation())
	{
	//The excision, Cut 2: sync died with the importers — it returns as orchestrator-owned
	//backend sync (P14, refactor_design/HANDOVER-excision.md). Fail loudly, never silently.
	case FileEvent::FileSyncData:
		emit progress(tr("Synchronizing Data Set"), 0);
		event->setComplete(false, FileEvent::notSupportedInNeoMsg(tr("Synchronizing with an external data file")));
		break;

	case FileEvent::FileOpen: // FileNew never reaches the loader (MainWindow special-cases it)
		emit progress(tr("Loading Data Set"), 0);
		emit beginLoad(event);
		break;

	//The excision, Cut 2: the exporter family is deleted. .jasp persistence, data export and
	//results export all return as NEO-era reimplementations (lane conversions / results work).
	case FileEvent::FileSave:
		emit progress(tr("Saving Data Set"), 0);
		event->setComplete(false, FileEvent::notSupportedInNeoMsg(tr("Saving .jasp files")));
		break;

	case FileEvent::FileExportResults:
		emit progress(tr("Exporting Result Set"), 0);
		event->setComplete(false, FileEvent::notSupportedInNeoMsg(tr("Exporting results")));
		break;

	case FileEvent::FileExportData:
	case FileEvent::FileGenerateData:
		emit progress(tr("Exporting Data Set"), 0);
		event->setComplete(false, FileEvent::notSupportedInNeoMsg(tr("Exporting data to a file")));
		break;

	case FileEvent::FileClose:
		event->setComplete();
		break;
	}
}

void AsyncLoader::loadTask(FileEvent *event)
{
	_currentEvent = event;

	if (event->isOnlineNode())
		QMetaObject::invokeMethod(_odm, "beginDownloadFile", Qt::AutoConnection, Q_ARG(QString, event->path()), Q_ARG(QString, "asyncloader"));
	else
		this->loadPackage("asyncloader");
}

void AsyncLoader::loadPackage(QString id)
{
	if (id != "asyncloader")
		return;

	OnlineDataNode *dataNode = nullptr;

	try
	{
		JASPTIMER_RESUME(AsyncLoader::loadPackage);
		Log::log()  << "AsyncLoader::loadPackage(" << id.toStdString() << ")" << std::endl;
		string path = fq(_currentEvent->path());

		if (_currentEvent->isOnlineNode()) //The OSF download finished; check it and resolve the local path.
		{
			dataNode = _odm->getActionDataNode(id);

			if (dataNode != nullptr && dataNode->error())
				throw LoaderException(fq(dataNode->errorMessage()));

			path = fq(_odm->getLocalPath(_currentEvent->path()));
		}

		string extension;
		if (_currentEvent->isDatabase())
		{
			//The excision, Cut 2: the database importer is gone; database data sources return
			//in a later NEO era.
			throw LoaderException(fq(FileEvent::notSupportedInNeoMsg(tr("Opening a database data source"))));
		}
		else
			extension = getExtension(path, extension); //Because it might still be ""...

		//NEO data plane: CSV-family bytes are owned by the orchestrator's data lane — the
		//data_open work (DataSetPackage::neoOpenDataset) has jasp-data-runner convert them to
		//Arrow (the lane sniffs the delimiter, so .txt/.tsv ride the same op). The frontend no
		//longer parses anything into its own database — the UI fetches views from the lane —
		//so a local open here is just a fresh empty dataset: no import, no MD5 pass over the
		//file, no external-synch watcher. Online (OSF) nodes fail clearly too until the lane
		//learns their paths (a later era; their MD5 wiring rides along then).
		// The excision aftermath (2026-09-02): boost::iequals died — a lowercased QString
		// compare (the suffixes are plain ASCII).
		const QString extensionLower = QString::fromStdString(extension).toLower();
		const bool laneOwned = !_currentEvent->isOnlineNode()
			&& (extensionLower == ".csv" || extensionLower == ".txt" || extensionLower == ".tsv");

		if (!laneOwned)
			throw LoaderException(fq(FileEvent::notSupportedInNeoMsg(tr("Opening %1 files").arg(extension.empty() ? QString("this type of") : QString::fromStdString(extension)))));

		Log::log() << "NEO: '" << path << "' is loaded by the data lane — frontend import deleted." << std::endl;
		//NEO: just register the (empty legacy skeleton) dataset in the workspace so the
		//tab/model wiring exists; the grid itself is served by the orchestrator lane.
		DataSetPackage * pkg = DataSetPackage::pkg();
		pkg->createDataSet();

		//The workspace table model was mutated on this (worker) thread, so let the GUI thread
		//know it must refresh its views (dataset tabbuttons etc.).
		emit dataSetsChanged();

		if (DataSet * bookkeepingDataSet = pkg->dataSet())
		{
			pkg->setInitialMD5("");

			if (_currentEvent->type() != Utils::FileType::jasp)
			{
				QFileInfo fileInfo(_currentEvent->path());
				long timestamp = fileInfo.isFile() ? fileInfo.lastModified().toSecsSinceEpoch() : 0;

				bookkeepingDataSet->setDataFileAndTimeStamp(_currentEvent->path().toStdString(), timestamp);
				bookkeepingDataSet->setDatabaseJson(_currentEvent->database());
			}

			pkg->setId(path);
			pkg->setFileReadOnly(_currentEvent->isReadOnly());
			_currentEvent->setDataFilePath(QString::fromStdString(bookkeepingDataSet->dataFilePath()));
		}
		_currentEvent->setComplete();

		if (dataNode != nullptr)
			_odm->deleteActionDataNode(id);
	}
	catch (LoaderException & e)
	{
		Log::log() << "Loader Exception in loadPackage: " << e.what() << std::endl;

		DataSetPackage::pkg()->deleteWorkspace(false); //Make sure we dont keep failed stuff in memory

		if (dataNode != nullptr)
			_odm->deleteActionDataNode(id);
		_currentEvent->setComplete(false, e.what(), e.cancelled);
	}
	catch (exception & e)
	{
		Log::log() << "Exception in loadPackage: " << e.what() << std::endl;

		DataSetPackage::pkg()->deleteWorkspace(true); //Make sure we dont keep failed stuff in memory

		if (dataNode != nullptr)
			_odm->deleteActionDataNode(id);
		_currentEvent->setComplete(false, e.what());
	}

	JASPTIMER_STOP(AsyncLoader::loadPackage);
	Log::log() << "[AsyncLoader::loadPackage] END" << std::endl;
}

void AsyncLoader::setOnlineDataManager(OnlineDataManager *odm)
{
	if (_odm != nullptr)
		disconnect(_odm, QOverload<QString>::of(&OnlineDataManager::downloadFileFinished),	this, &AsyncLoader::loadPackage);

	_odm = odm;

	if (_odm != nullptr)
		connect(_odm, QOverload<QString>::of(&OnlineDataManager::downloadFileFinished), this, &AsyncLoader::loadPackage, Qt::QueuedConnection);
}
