//
// Copyright (C) 2013-2018 University of Amsterdam
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

#include "fileevent.h"
#include "dataset.h"
#include "log.h"

#include <QTimer>
#include "processinfo.h"
#include "utilities/appdirs.h"


void FileEvent::setSyncDataSet(DataSet * ds)			
{ 
	_syncDataSet = ds; 
	Log::log() << "[FileEvent::setSyncDataSet] Set syncDataSet to: " << (ds ? QString::number(ds->id()) : "NULL") << std::endl; 
}

DataSet * FileEvent::syncDataSet() const
{
	DataSet * result = _syncDataSet ? static_cast<DataSet*>(_syncDataSet.data()) : nullptr;
	Log::log() << "[FileEvent::syncDataSet] Returning: " << (result ? QString::number(result->id()) : "NULL") << std::endl;
	return result;
}

FileEvent::FileEvent(QObject *parent, FileEvent::FileMode fileMode)
	: QObject(parent), _operation(fileMode)
{
	// NEO (the excision, Cut 2): the exporter family is deleted — saving/exporting returns
	// as lane conversions in a later era. Save-ish modes now fail with a clear message in
	// AsyncLoader::io instead of instantiating an Exporter here.
}

void FileEvent::setDataFilePath(const QString & path)
{
	_dataFilePath = path;
}

void FileEvent::setDatabase(const Json::Value & dbInfo)
{
	_database = dbInfo;
	Log::log() << "[FileEvent::setDatabase] Database set" << std::endl;
}

bool FileEvent::setPath(const QString & path)
{
	_path = path;
	_type = Utils::getTypeFromFileName(path.toStdString());

	// NEO (the excision, Cut 2): exporter-driven type negotiation (default extension,
	// allowed-type validation) died with the exporter family. Save-ish events fail later
	// with a clear "not supported in NEO yet" message; nothing else needed this branch.

	return true;
}

void FileEvent::setComplete(bool success, const QString & message, bool cancelled)
{
	_completed	= true;
	_success	= success;
	_message	= message;
	_cancelled	= cancelled;

	Log::log() << "[FileEvent::setComplete] operation=" << _operation << ", success=" << success << ", message=" << message.toStdString() << std::endl;

	emit completed(this);
}

void FileEvent::chain(FileEvent *event)
{
	_chainedTo = event;
	connect(event, &FileEvent::completed, this, &FileEvent::chainedComplete);
}

bool FileEvent::isExample() const
{
	return path().startsWith(AppDirs::examples());
}

bool FileEvent::autoSaveExists()
{
	return QFileInfo::exists(pathTmp());
}

void FileEvent::removeAutoSaveIfItExists()
{
	if(!autoSaveExists())
		return;
	
	QFile autoSaveFile(pathTmp());
	
	autoSaveFile.remove();
}

QString FileEvent::pathTmp()
{
	static QString tmpFile = []()
	{
		QDir autoSaveFolder(AppDirs::autoSaveDir());
		
		QFileInfoList files = autoSaveFolder.entryInfoList(QDir::Filter::Files | QDir::NoDotAndDotDot | QDir::NoSymLinks);
		
		std::set<QString> usedNames;
		
		for(QFileInfo & fi : files)
			usedNames.insert(fi.fileName());
		
		auto aNamePlease = [](){
			static int num = 0;
			return "autosave-" + QString::number(ProcessInfo::currentPID()) + "-" + QString::number(num++) + ".jasp";
		};
		
		QString newFileName;
		
		do		{ newFileName = aNamePlease();	}
		while	( usedNames.count(newFileName)	);
		
		return autoSaveFolder.absoluteFilePath(newFileName);
		
	}();
	
	return tmpFile;
}

const std::string FileEvent::databaseStr() const 
{ 
	return _database.toStyledString();
}

QString FileEvent::getProgressMsg() const
{
	//jasp = 0, html, csv, txt, tsv, sav, zsav, ods, xls, xlsx, pdf, sas7bdat, sas7bcat, por, xpt, dta, database, rdata, rds, empty, unknown
	switch(_operation)
	{
	case FileEvent::FileOpen:
		switch(_type)
		{
		case Utils::FileType::csv:
		case Utils::FileType::txt:
		case Utils::FileType::tsv:
		case Utils::FileType::ods:			return tr("Importing Data from %1").arg(FileTypeBaseToQString(_type).toUpper());
		case Utils::FileType::xls:
		case Utils::FileType::xlsx:			return tr("Importing Excel File");
        case Utils::FileType::sav:
		case Utils::FileType::zsav:
		case Utils::FileType::por:			return tr("Importing SPSS File");
		case Utils::FileType::xpt:
		case Utils::FileType::sas7bdat:
		case Utils::FileType::sas7bcat:		return tr("Importing SAS File");
		case Utils::FileType::dta:			return tr("Importing STATA File");
		case Utils::FileType::jasp:			return tr("Loading JASP File");
		case Utils::FileType::rdata:
		case Utils::FileType::rds:			return tr("Loading R Data File");
		case Utils::FileType::mwx:
		case Utils::FileType::mpx:			return tr("Loading Minitab Data Workbook File");
		default:							return tr("Loading File");
		}
		break;

	case FileEvent::FileSave:			return !_tmp ? tr("Saving JASP File") : tr("Saving autosave JASP File");
	case FileEvent::FileExportResults:	return tr("Exporting Results");
	case FileEvent::FileExportData:
	case FileEvent::FileGenerateData:	return tr("Exporting Data");
	default:							break;
	}

	return tr("Processing File"); //This will never show up on screen right?
}

void FileEvent::setSilent(bool newSilent)
{
	_cancelled = newSilent;
}

QString FileEvent::notSupportedInNeoMsg(const QString & what)
{
	// The excision, Cut 2: one shared, HONEST message for every removed route — never a
	// silent nothing. Each feature here returns as a NEO-era reimplementation
	// (refactor_design/HANDOVER-excision.md).
	return QObject::tr("%1 is not supported in this NEO rebuild of JASP yet — it returns in a later era. "
					  "For now only the CSV family (.csv/.txt/.tsv) can be opened, through the data lane.").arg(what);
}

