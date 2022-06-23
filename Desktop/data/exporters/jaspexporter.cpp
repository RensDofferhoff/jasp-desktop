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

#include "jaspexporter.h"

#include <sys/stat.h>

#include <ios>
#include <archive.h>
#include <archive_entry.h>
#include <json/json.h>
#include <fstream>
#include "archivereader.h"
#include "version.h"
#include "tempfiles.h"
#include "appinfo.h"
#include "log.h"
#include "utilenums.h"
#include "utilities/qutils.h"
#include "data/databaseconnectioninfo.h"
#include <fstream>
#include "columnutils.h"

const Version JASPExporter::jaspArchiveVersion = Version("4.0.0");


JASPExporter::JASPExporter()
{
	_defaultFileType = Utils::FileType::jasp;
	_allowedFileTypes.push_back(Utils::FileType::jasp);
}

void JASPExporter::saveDataSet(const std::string &path, boost::function<void(int)> progressCallback)
{
	struct archive *a;

	a = archive_write_new();
	archive_write_set_format_zip(a);
	archive_write_set_compression_xz(a);
    
	if (archive_write_open_filename(a, path.c_str()) != ARCHIVE_OK)
		throw std::runtime_error("File could not be opened.");

	createManifest(a);
	saveDatabase(a);
	saveAnalyses(a);
	saveResults(a);

	if (archive_write_close(a) != ARCHIVE_OK)
		throw std::runtime_error("File could not be closed.");

	archive_write_free(a);

	progressCallback(100);
}

void JASPExporter::saveResults(archive * a)
{
	DataSetPackage::pkg()->waitForExportResultsReady();

	//Create new entry for archive: HTML results
	QByteArray		html		= DataSetPackage::pkg()->analysesHTML().toUtf8();
	size_t			htmlSize	= html.size();
	archive_entry *	entry		= archive_entry_new();
	
	archive_entry_set_pathname(	entry,	"index.html");
	archive_entry_set_size(		entry,	int(htmlSize));
	archive_entry_set_filetype(	entry,	AE_IFREG);
	archive_entry_set_perm(		entry,	0644);  //basically chmod

				archive_write_header(   a,  entry);
	size_t ws = archive_write_data(		a, html.data(), htmlSize);

	if (ws != size_t(htmlSize))
		throw std::runtime_error("Can't save jasp archive writing ERROR");

	archive_entry_free(entry);							   
}

void JASPExporter::saveAnalyses(archive *a)
{
	archive_entry *entry;
	
		const Json::Value &analysesJson = DataSetPackage::pkg()->analysesData();

		//Create new entry for archive NOTE: must be done before data is added
		std::string analysesString			= analysesJson.toStyledString();
		size_t		sizeOfAnalysesString	= analysesString.size();
					entry					= archive_entry_new();
					
		archive_entry_set_pathname(	entry,	"analyses.json");
		archive_entry_set_size(		entry,	int(sizeOfAnalysesString));
		archive_entry_set_filetype(	entry,	AE_IFREG);
		archive_entry_set_perm(		entry,	0644);  //basically chmod
		archive_write_header(		a,		entry);

		archive_write_data(a, analysesString.c_str(), sizeOfAnalysesString);

		archive_entry_free(entry);

		char imagebuff[8192];

		Json::Value analysesDataList = analysesJson;
		if (!analysesDataList.isArray())
			analysesDataList = analysesJson["analyses"];

		for (Json::Value::iterator iter = analysesDataList.begin(); iter != analysesDataList.end(); iter++)
		{
			Json::Value &analysisJson = *iter;
			std::vector<std::string> paths = TempFiles::retrieveList(analysisJson["id"].asInt());
			for (size_t j = 0; j < paths.size(); j++)
			{
				// std::ios::ate seeks to the end of stream immediately after open
				std::ifstream readTempFile(TempFiles::sessionDirName() + "/" + paths[j], std::ios::ate | std::ios::binary);

				if (readTempFile.is_open())
				{
					int imageSize = readTempFile.tellg();		// get size from curpos

					entry = archive_entry_new();
					archive_entry_set_pathname(entry, paths[j].c_str());
					
					archive_entry_set_size(		entry,	imageSize);
					archive_entry_set_filetype(	entry,	AE_IFREG);
					archive_entry_set_perm(		entry,	0644); // Not sure what this does
					archive_write_header(		a,		entry);

					int	bytes = 0;
					readTempFile.seekg(0, std::ios::beg);		// move back to begin

					while (!readTempFile.eof())
					{
						readTempFile.read(imagebuff, sizeof(imagebuff));
						bytes = readTempFile.gcount();

						archive_write_data(a, imagebuff, size_t(bytes));
					}

					archive_entry_free(entry);

				}
				else
					Log::log() << "JASP Export: cannot find/open file " << (TempFiles::sessionDirName() + "/" + paths[j]);

				readTempFile.close();
			}
		}	
}

void JASPExporter::saveDatabase(archive * a)
{
	???
}



void JASPExporter::createManifest(archive *a)
{
	struct archive_entry *entry = archive_entry_new();

	std::stringstream manifestStream;
	manifestStream << "Manifest-Version: 1.0" << "\n";
	manifestStream << "Created-By: " << AppInfo::getShortDesc() << "\n";
	manifestStream << "JASP-Archive-Version: " << jaspArchiveVersion.asString() << "\n";

	manifestStream.flush();

	const std::string& tmp	= manifestStream.str();
	size_t manifestSize		= tmp.size();
	const char* manifest	= tmp.c_str();

	archive_entry_set_pathname(	entry,	"META-INF/MANIFEST.MF");
	archive_entry_set_size(		entry,	int(manifestSize));
	archive_entry_set_filetype(	entry,	AE_IFREG);
	archive_entry_set_perm(		entry,	0644); // Not sure what this does
	archive_write_header(		a,		entry);

	archive_write_data(a, manifest, manifestSize);

	archive_entry_free(entry);
}



