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
#include "datasetloader.h"

#include <boost/algorithm/string.hpp>

#include "importers/databaseimporter.h"
#include "importers/csvimporter.h"
#include "importers/jaspimporter.h"
#include "importers/odsimporter.h"
#include "importers/readstatimporter.h"
#include "importers/excelimporter.h"
#include "importers/rdataimporter.h"
#include "importers/minitabimporter.h"
#include "asyncloader.h"

#include <QFileInfo>

#include "timers.h"
#include "utils.h"
#include "log.h"
#include "utilities/desktopcommunicator.h"
#include "datasetpackage.h"

using namespace std;
using namespace ods;

string DataSetLoader::getExtension(const string &locator, const string &extension)
{
	std::filesystem::path path(locator);
	string ext = path.extension().generic_string();

	if (!ext.length()) ext=extension;
	return ext;
}

Importer* DataSetLoader::getImporter(const string & locator, const string &ext)
{
	if(	ext == "DATABASE")									return new DatabaseImporter();
	if(	boost::iequals(ext,".csv") || 
		boost::iequals(ext,".txt") ||
		boost::iequals(ext,".tsv"))							return new CSVImporter();
	if(	boost::iequals(ext,".ods"))							return new ODSImporter();
	if( boost::iequals(ext,".xls") ||
		boost::iequals(ext,".xlsx"))						return new ExcelImporter();
	if(	ReadStatImporter::extSupported(ext))				return new ReadStatImporter(ext);
	if( boost::iequals(ext,".rdata") ||
		boost::iequals(ext,".rds"))							return new RDataImporter();
	if( boost::iequals(ext, ".mwx") ||
		boost::iequals(ext,".mpx"))							return new MinitabImporter();

	return nullptr; //If NULL then JASP will try to load it as a .jasp file (if the extension matches)
}

void DataSetLoader::loadPackage(const string &locator, const string &extension, std::function<void(int)> progress)
{
	JASPTIMER_RESUME(DataSetLoader::loadPackage);

	Importer* importer = getImporter(locator, extension);

	if (importer)
	{
		DataSet * dataSet = DataSetPackage::pkg()->createDataSet();
		importer->loadDataSet(locator, dataSet, progress);
		char chosenDelimiter = DesktopCommunicator::singleton()->knownCsvDelimiter();
		if (chosenDelimiter != '\0' && dataSet)
			dataSet->setCsvDelimiter(chosenDelimiter);
		DesktopCommunicator::singleton()->setKnownCsvDelimiter('\0');
		delete importer;
		DataSetPackage::pkg()->workspace()->setShownDataSet(dataSet);
		DataSetPackage::pkg()->workspace()->refresh();
	}
	else if(extension == ".jasp" || extension == "jasp")
		JASPImporter::loadDataSet(locator, progress);
	else
		throw LoaderException("JASP does not support loading the file-type \"" + extension + '"');

	JASPTIMER_STOP(DataSetLoader::loadPackage);

}

void DataSetLoader::syncPackage(const string &locator, const string &extension, DataSet * dataSet, std::function<void(int)> progress)
{
	Log::log() << "[DataSetLoader::syncPackage] START: locator=" << locator << ", extension=" << extension << ", dataSetId=" << (dataSet ? dataSet->id() : -1) << std::endl;

	Importer* importer = getImporter(locator, extension);

	if (importer)
	{
		Log::log() << "[DataSetLoader::syncPackage] Importer found, calling importer->syncDataSet()" << std::endl;
		if (dataSet)
		{
			Log::log() << "[DataSetLoader::syncPackage] dataSet->csvDelimiter()=" << dataSet->csvDelimiter() << std::endl;
			DesktopCommunicator::singleton()->setKnownCsvDelimiter(dataSet->csvDelimiter());
		}
		importer->syncDataSet(locator, dataSet, progress);
		DesktopCommunicator::singleton()->setKnownCsvDelimiter('\0');
		delete importer;
		Log::log() << "[DataSetLoader::syncPackage] importer->syncDataSet() returned" << std::endl;
	}
	else
	{
		Log::log() << "[DataSetLoader::syncPackage] No importer found for extension=" << extension << std::endl;
	}
	Log::log() << "[DataSetLoader::syncPackage] END" << std::endl;
}
