#include "importdataset.h"
#include "timers.h"
#include "appinfo.h"

using namespace std;

ImportDataSet::ImportDataSet(Importer *importer) : _importer(importer)
{
}

ImportDataSet::~ImportDataSet()
{
	JASPTIMER_SCOPE(ImportDataSet::~ImportDataSet());
	
	for (ImportColumn * col : _columns)
		delete col;
}

void ImportDataSet::addColumn(ImportColumn *column)
{
	_columns.push_back(column);
}

size_t ImportDataSet::columnCount() const
{
	return _columns.size();
}

const string & ImportDataSet::description() const
{
	static std::string localCache;
	localCache = "Originally imported into " + AppInfo::getShortDesc()+ " on " + Utils::currentDateTime();	
	return localCache;
}

size_t ImportDataSet::rowCount() const
{
	if (columnCount() == 0)
		return 0;
	else
	{
		ImportColumn* col = *(_columns.begin());
		return col->size();
	}
}

ImportColumn* ImportDataSet::getColumn(string name) const
{
	if (_nameToColMap.empty())
		throw runtime_error("Cannot call ImportDataSet::getColumn() before ImportDataSet::buildDictionary()");
	else
		return _nameToColMap.find(name)->second;
}

ImportColumns::iterator ImportDataSet::begin()
{
	return _columns.begin();
}

ImportColumns::iterator ImportDataSet::end()
{
	return _columns.end();
}

ImportColumns::reverse_iterator ImportDataSet::rbegin()
{
	return _columns.rbegin();
}

ImportColumns::reverse_iterator ImportDataSet::rend()
{
	return _columns.rend();
}

void ImportDataSet::clear()
{
	clearColumns();
	_nameToColMap.clear();
}

void ImportDataSet::erase(ImportColumns::iterator it)
{
	_columns.erase(it);
}

void ImportDataSet::buildDictionary()
{
	_nameToColMap.clear();
	for(ImportColumn * col : *this)
		if(col->name() != "")
			_nameToColMap[col->name()] = col;

	//Lets name the unnamed columns the same way csv does
	size_t curCol = 0;

	for(ImportColumn * col : *this)
	{
		curCol++;
		
		if(col->name() == "")
		{
			std::string newName;
			do
				newName = "V" + std::to_string(curCol);
			while(_nameToColMap.count(newName) > 0);
				
			col->setName(newName);

			_nameToColMap[col->name()] = col;
		}
	}
}

