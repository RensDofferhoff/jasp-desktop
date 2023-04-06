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

#include "jaspimporter.h"
#include "columnutils.h"
#include <fstream>

#include <sys/stat.h>

#include <fcntl.h>

//#include "libzip/config.h"
#include <archive.h>
#include <archive_entry.h>
#include <json/json.h>
#include "archivereader.h"
#include "tempfiles.h"
#include "../exporters/jaspexporter.h"

#include "resultstesting/compareresults.h"
#include "log.h"

void JASPImporter::loadDataSet(const std::string &path, boost::function<void(int)> progressCallback)
{	
	JASPTIMER_RESUME(JASPImporter::loadDataSet INIT);

	DataSetPackage * packageData = DataSetPackage::pkg();

	packageData->setIsArchive(true);

	readManifest(path);

	Compatibility compatibility = isCompatible();

	if (compatibility == JASPImporter::NotCompatible)	throw std::runtime_error("The file version is too new.\nPlease update to the latest version of JASP to view this file.");
	else if (compatibility == JASPImporter::Limited)	packageData->setWarningMessage("This file was created by a newer version of JASP and may not have complete functionality.");

	JASPTIMER_STOP(JASPImporter::loadDataSet INIT);

	packageData->beginLoadingData();
	loadDataArchive(path, progressCallback);
	loadJASPArchive(path, progressCallback);
	packageData->endLoadingData();
}

void JASPImporter::loadDataArchive_1_00(const std::string &path, boost::function<void(int)> progressCallback)
{
	JASPTIMER_SCOPE(JASPImporter::loadDataArchive_1_00);

	DataSetPackage * packageData = DataSetPackage::pkg();
	bool success = false;

	hier moet je database uitpakken naar tempfiles

	if(resultXmlCompare::compareResults::theOne()->testMode())
	{
		//Read the results from when the JASP file was saved and store them in compareResults field

		ArchiveReader	resultsEntry	= ArchiveReader(path, "index.html");
		int				errorCode		= 0;
		std::string		html			= resultsEntry.readAllData(sizeof(char), errorCode);

		if (errorCode != 0)
			throw std::runtime_error("Could not read result from 'index.html' in JASP archive.");

		resultXmlCompare::compareResults::theOne()->setOriginalResult(QString::fromStdString(html));
	}
}

void JASPImporter::loadJASPArchive(const std::string &path, boost::function<void(int)> progressCallback)
{
	if (DataSetPackage::pkg()->archiveVersion().major() == 4)
		loadJASPArchive_1_00(path, progressCallback);
	else
		throw std::runtime_error("The file version is not supported (too new).\nPlease update to the latest version of JASP to view this file.");
}

void JASPImporter::loadJASPArchive_1_00(const std::string &path, boost::function<void(int)> progressCallback)
{
	JASPTIMER_SCOPE(JASPImporter::loadJASPArchive_1_00 read analyses.json);
	Json::Value analysesData;

	progressCallback(66); // "Loading Analyses",

	if (parseJsonEntry(analysesData, path, "analyses.json", false))
	{
		std::vector<std::string> resources = ArchiveReader::getEntryPaths(path, "resources");
	
		double resourceCounter = 0;
		for (std::string resource : resources)
		{
			ArchiveReader resourceEntry = ArchiveReader(path, resource);
			std::string filename	= resourceEntry.fileName();
			std::string dir			= resource.substr(0, resource.length() - filename.length() - 1);

			std::string destination = TempFiles::createSpecific(dir, resourceEntry.fileName());

			deze code moet naar losse functie a la JASPExporter

			JASPTIMER_RESUME(JASPImporter::loadJASPArchive_1_00 Write file stream);
			std::ofstream file(destination.c_str(),  std::ios::out | std::ios::binary);

			static char streamBuff[8192 * 32];
			file.rdbuf()->pubsetbuf(streamBuff, sizeof(streamBuff)); //Set the buffer manually to make it much faster our issue https://github.com/jasp-stats/INTERNAL-jasp/issues/436 and solution from:  https://stackoverflow.com/a/15177770
	
			static char copyBuff[8192 * 4];
			int			bytes		= 0,
						errorCode	= 0;

			do
			{
				JASPTIMER_RESUME(JASPImporter::loadJASPArchive_1_00 Write file stream - read data);
				bytes = resourceEntry.readData(copyBuff, sizeof(copyBuff), errorCode);
				JASPTIMER_STOP(JASPImporter::loadJASPArchive_1_00 Write file stream - read data);

				if(bytes > 0 && errorCode == 0)
				{
					JASPTIMER_RESUME(JASPImporter::loadJASPArchive_1_00 Write file stream - write to stream);
					file.write(copyBuff, bytes);
					JASPTIMER_STOP(JASPImporter::loadJASPArchive_1_00 Write file stream - write to stream);
				}
				else break;
			}
			while (true);

			file.flush();
			file.close();
			JASPTIMER_STOP(JASPImporter::loadJASPArchive_1_00 Write file stream);
	
			if (errorCode != 0)
				throw std::runtime_error("Could not read resource files.");

			JASPTIMER_STOP(JASPImporter::loadJASPArchive_1_00 Create resource files);

			progressCallback( 67 + int((33.0 / double(resources.size())) * ++resourceCounter));// "Loading Analyses",
		}
	}

	JASPTIMER_STOP(JASPImporter::loadJASPArchive_1_00 read analyses.json);
	
	JASPTIMER_RESUME(JASPImporter::loadJASPArchive_1_00 packageData->setAnalysesData(analysesData));
	DataSetPackage::pkg()->setAnalysesData(analysesData);
	JASPTIMER_STOP(JASPImporter::loadJASPArchive_1_00 packageData->setAnalysesData(analysesData));

	progressCallback(100); //"Initializing Analyses & Results",
}


void JASPImporter::readManifest(const std::string &path)
{
	bool            foundVersion		= false;
	std::string     manifestName		= "manifest.json";
	ArchiveReader	manifestReader		= ArchiveReader(path, manifestName);
	int             size				= manifestReader.bytesAvailable(),
		errorCode;

	if (size > 0)
	{
		std::string manifestStr = manifestReader.readAllData(sizeof(char), errorCode);

		if (errorCode != 0)
			throw std::runtime_error("Could not read manifest of JASP archive.");

		Json::Reader    parser;
		Json::Value     manifest;
		parser.parse(manifestStr, manifest);

		manifestStr = manifest.get("jaspArchiveVersion", "").asString();

		foundVersion = ! manifestStr.empty();

		DataSetPackage::pkg()->setArchiveVersion(Version(manifestStr));
	}

	if ( ! foundVersion)
		throw std::runtime_error("Archive missing version information.");
}

bool JASPImporter::parseJsonEntry(Json::Value &root, const std::string &path,  const std::string &entry, bool required)
{
	//Not particularly happy about the way we need to add a delete at every return here. Would be better to not use `new` and just instantiate a scoped var
	//But that would require removing some `std::runtime_error` from `openEntry` in the `ArchiveReader` constructor. 
	// And this is not the time to rewrite too many things.
	ArchiveReader * dataEntry = NULL;
	try
	{
		dataEntry = new ArchiveReader(path, entry);
	}
	catch(...)
	{
		return false;
	}
	
	if (!dataEntry->archiveExists())
	{
		delete dataEntry;
		throw std::runtime_error("The selected JASP archive '" + path + "' could not be found.");
	}

	if (!dataEntry->exists())
	{
		delete dataEntry;
		
		if (required)
			throw std::runtime_error("Entry '" + entry + "' could not be found in JASP archive.");

		return false;
	}

	int size = dataEntry->bytesAvailable();
	if (size > 0)
	{
		char *data = new char[size];
		int startOffset = dataEntry->pos();
		int errorCode = 0;
		while (dataEntry->readData(&data[dataEntry->pos() - startOffset], 8016, errorCode) > 0 && errorCode == 0) ;

		if (errorCode < 0)
		{
			delete dataEntry;
			throw std::runtime_error("Could not read Entry '" + entry + "' in JASP archive.");
		}

		Json::Reader jsonReader;
		jsonReader.parse(data, (char*)(data + (size * sizeof(char))), root);

		delete[] data;
	}

	dataEntry->close();

	delete dataEntry;
	return true;
}

JASPImporter::Compatibility JASPImporter::isCompatible()
{
	if (DataSetPackage::pkg()->archiveVersion().major()		> JASPExporter::jaspArchiveVersion.major() )
		return JASPImporter::NotCompatible;

	if (DataSetPackage::pkg()->archiveVersion().minor()		> JASPExporter::jaspArchiveVersion.minor() )
		return JASPImporter::Limited;

	return JASPImporter::Compatible;
}



