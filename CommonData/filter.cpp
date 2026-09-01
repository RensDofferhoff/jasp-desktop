#include "log.h"
#include <cassert>
#include "filter.h"
#include <atomic>
#include "timers.h"
#include "qutils.h"
#include "dataset.h"
#include "columnencoder.h"
#include "jsonutilities.h"
// The excision, Cut 6: filtereddata.h/varinfomodelproxy.h died with Filter's provider role
// and per-row mask — forms are served by the Workspace-injected provider; filters return as
// derived boolean columns (HANDOVER-excision.md).
// The excision, Cut 5: labelfiltergenerator.h died with Label/Column — the generated-filter
// machinery returns with the labels editor (B2) on the jasp:labels overlay.

// The excision, Cut 3: DatabaseInterface is gone; Filter ids are minted from this
// process-global counter (computed datasets key their input by defaultInputFilterId).
static std::atomic<int> g_nextFilterId{1};

Filter::Filter(DataSet * data)
: DataSetBaseNode(dataSetBaseNodeType::filter, data),
  _data(				data),
  _name(				DEFAULT_FILTER_NAME),
  _constructorJson(	DEFAULT_FILTER_JSON),
  _generatedFilter(	DEFAULT_FILTER_GEN)
{
	assert(_data);

	// The excision, Cut 3: the sqlite filter row is gone — mint the id locally.
	_id					= g_nextFilterId++;
	_rFilter			= fq(defaultRFilter());

	connectionCreation();
}

Filter::Filter(DataSet * data, const std::string & name, bool createIfMissing)
: DataSetBaseNode(dataSetBaseNodeType::filter),
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

	// The excision, Cut 6: the infoSignaller()/varInfo() relays died with Filter's
	// VariableInfoProvider role — ColumnsModel::bindLane owns that wiring now.
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
	bool	wasChange			=_constructorJson != constructorJson;
			_constructorJson	= constructorJson;

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
	bool	wasChange	= _invalidated != invalidated;
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
	// The excision, Cut 6: the per-row mask reset died with the mask — v1 has no filter
	// compaction; everything passes until filters return as derived boolean columns.
	incRevision();
}

void Filter::rescanForColumns()
{
	_columnsUsedInRFilter		= data()->findUsedColumnNames(_rFilter);
	_columnsInConstructorJson	= JsonUtilities::convertDragNDropFilterJSONToSet(_constructorJson);
}

void Filter::datasetChanged(int, QStringList changedColumns, QStringList missingColumns, QMap<QString, QString> changeNameColumns, bool rowCountChanged, bool)
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

		//Just reset the filter while the user gets the chance to fix their now broken filter
		reset();

		emit refreshAllAnalyses(this);
		data()->resetFilterCounters();

		//The following errormsg is overwritten immediately but that is because constructorJson changed triggers qml which triggers (some vents later) a send event. So yeah...
		//Ill leave it here though because it would be nice to show this friendlier msg then "null not found"
		setFilterErrorMsgQ(tr("Some columns were removed from the data and your filter(s)!"));
	}

	if(invalidateMe)
		setInvalidated(true);

	// The excision, Cut 6: the per-row mask resize and the infoSignaller variable relays died
	// with Filter's provider role — ColumnsModel::bindLane relays schema changes to forms now.
}

const std::string &Filter::generatedFilter() const
{
	return _generatedFilter;
}

QString Filter::constructorRQ() const
{
	return tq(_constructorR);
}

QString Filter::rFilterQ() const
{
	return tq(_rFilter);
}

QString Filter::nameQ() const
{
	return tq(_name);
}

QString Filter::filterErrorMsgQ() const
{
	return tq(_errorMsg);
}

QString Filter::generatedFilterQ() const
{
	return tq(_generatedFilter);
}

QString Filter::constructorJsonQ() const
{
	return tq(_constructorJson);
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
	setConstructorJson(	fq(newconstructorJson));
}

void Filter::setFilterErrorMsgQ(const QString &newFilterErrorMsg)
{
	setErrorMsg(	fq(newFilterErrorMsg));
}

void Filter::setStatusBarText(const QString &newStatusBarText)
{
	_statusBarText  = newStatusBarText;
}
