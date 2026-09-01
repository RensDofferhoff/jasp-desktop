#include <QMap>
#include "filtermodel.h"
#include "datasetpackage.h"
#include "filter.h"
#include "qutils.h"
#include "log.h"

FilterModel::FilterModel(QObject * parent)
	: QObject(parent)
{

	connect(DataSetPackage::pkg(), &DataSetPackage::shownDataSetChanged,	this, &FilterModel::filterChanged);
	connect(DataSetPackage::pkg(), &DataSetPackage::filtersCountChanged,	this, &FilterModel::filterDropDownListChanged,	Qt::QueuedConnection);
	connect(DataSetPackage::pkg(), &DataSetPackage::shownFilterChanged,		this, &FilterModel::filterChanged									);
	connect(DataSetPackage::pkg(), &DataSetPackage::shownFilterChanged,		this, &FilterModel::filterDropDownListChanged,	Qt::QueuedConnection);
}

Filter *FilterModel::filter() const
{
	return DataSetPackage::filter();
}


bool FilterModel::isJustGeneratedFilter() const
{
	return filter() && filter()->rFilter() == Filter::defaultRFilter() && filter()->constructorJson() == DEFAULT_FILTER_JSON;
}

void FilterModel::applyConstructorJson(QString newConstructorJson)
{
	Q_UNUSED(newConstructorJson);

	if(!filter())
		return;

	// The excision, Cut 4: the filter's undo command is gone with the legacy data route.
	// Filters return as derived boolean columns (HANDOVER-excision.md); until then the
	// editor is inert on NEO data.
	if (newConstructorJson != filter()->constructorJson())
		Log::log() << "FilterModel::applyConstructorJson: filters return as derived columns — ignored" << std::endl;
}

void FilterModel::applyRFilter(QString newRFilter)
{
	Q_UNUSED(newRFilter);

	if(!filter())
		return;

	if (newRFilter != filter()->rFilter())
		Log::log() << "FilterModel::applyRFilter: filters return as derived columns — ignored" << std::endl;
}

void FilterModel::resetRFilter()
{
	if(!filter())
		return;

	if (filter()->defaultRFilter() != filter()->rFilter())
		Log::log() << "FilterModel::resetRFilter: filters return as derived columns — ignored" << std::endl;
}


void FilterModel::processFilterResult(QString name)
{
	// The excision, Cut 6: the filter-result vector died with the per-row mask — engine
	// results land nowhere until filters return as derived boolean columns. Keep the slot
	// (it is signal-wired) but make it an honest no-op.
	Q_UNUSED(name);
	Log::log() << "FilterModel::processFilterResult: filter results return with derived columns — ignored" << std::endl;
}

void FilterModel::onFilterChanged()
{
	if(filter())
		setCurrentFilterId(filter()->id());
}

void FilterModel::computeColumnSucceeded(QString columnName, QString, bool dataChanged)
{
	if(!filter())
		return;

	if(dataChanged && filter()->columnUsed(columnName))
		filter()->setInvalidated(true);
}

QVariantList FilterModel::filterDropDownList() const
{
	typedef QMap<QString, QVariant> localMap;
	
	QVariantList out;
	
	if(DataSetPackage::pkg()->workspace())
	{
		//out.append(localMap{std::make_pair("value", tq("---")), std::make_pair("label", "---")});
		
		for(DataSet * dataSet : DataSetPackage::pkg()->workspace()->dataSets())
		{
			out.append(localMap{std::make_pair("value", tq(dataSet == DataSetPackage::pkg()->dataSet() ? "*" : "-")), std::make_pair("label", dataSet->title() + ":")});
			
			if(dataSet->defaultFilter())
				out.append(localMap{std::make_pair("value", tq(std::to_string(dataSet->defaultFilter()->id()))), std::make_pair("label", dataSet->defaultFilter()->title())});
			
			for(const Filter * f : dataSet->filters())
				if(f != dataSet->defaultFilter())
					out.append(localMap{std::make_pair("value", tq(std::to_string(f->id()))), std::make_pair("label", f->title())});
			
			out.append(localMap{std::make_pair("value", tq("---")), std::make_pair("label", tq(std::to_string(dataSet->id())))});
		}
	}
	
	return out;
}

QVariantList FilterModel::filterDropDownAnalysisList() const
{
	typedef QMap<QString, QVariant> localMap;
	
	QVariantList out;
	
	if(DataSetPackage::pkg()->workspace())
	{
		out.append(localMap{std::make_pair("value", tq("---")), std::make_pair("label", "---")});
		
		for(DataSet * dataSet : DataSetPackage::pkg()->workspace()->dataSets())
		{
			out.append(localMap{std::make_pair("value", tq(dataSet == DataSetPackage::pkg()->dataSet() ? "*" : "-")), std::make_pair("label", dataSet->title() + ":")});
			
			if(dataSet->defaultFilter())
				out.append(localMap{std::make_pair("value", tq(std::to_string(dataSet->defaultFilter()->id()))), std::make_pair("label", dataSet->defaultFilter()->title())});
			
			for(const Filter * f : dataSet->filters())
				if(f != dataSet->defaultFilter())
					out.append(localMap{std::make_pair("value", tq(std::to_string(f->id()))), std::make_pair("label", f->title())});
			
			out.append(localMap{std::make_pair("value", tq("---")), std::make_pair("label", "---")});
		}
	}
	
	return out;
}

QVariantList FilterModel::computeFilterDropDownList() const
{
	typedef QMap<QString, QVariant> localMap;
	
	QVariantList out;
	
	if(DataSet * dataSet = DataSetPackage::pkg()->dataSet())
	{
		if(dataSet->defaultFilter())
			out.append(localMap{std::make_pair("value", tq(dataSet->defaultFilter()->name())), std::make_pair("label", dataSet->defaultFilter()->title())});
		
		for(const Filter * f : dataSet->filters())
			if(f != dataSet->defaultFilter())
				out.append(localMap{std::make_pair("value", tq(f->name())), std::make_pair("label", f->title())});
	}
	
	return out;
}

bool FilterModel::filterVisible() const
{
	return _filterVisible;
}

void FilterModel::setFilterVisible(bool newFilterVisible)
{
	if (_filterVisible == newFilterVisible)
		return;
	_filterVisible = newFilterVisible;
		
	emit filterVisibleChanged();
}

bool FilterModel::showEasyFilter() const
{
	return _showEasyFilter;
}

void FilterModel::setShowEasyFilter(bool newShowEasyFilter)
{
	if (_showEasyFilter == newShowEasyFilter)
		return;
	_showEasyFilter = newShowEasyFilter;
	emit showEasyFilterChanged();
}

void FilterModel::reset()
{
	_showEasyFilter = true;
	_filterVisible  = false;
}

QString FilterModel::currentFilter() const
{
	return !filter() ? "" : tq(filter()->name());
}

int FilterModel::currentFilterId() const
{
	return !filter() ? -1 : filter()->id();
}

QString FilterModel::currentFilterTitle() const
{
	return !filter() ? "" : filter()->title();
}

void FilterModel::setCurrentFilterId(int id)
{
	DataSetPackage::pkg()->workspace()->showFilter(id);
	
	emit filterChanged();
	emit filterDropDownListChanged();
	
	DataSetPackage::pkg()->workspace()->refresh();
	
}

void FilterModel::renameCurrentFilter(const QString &newName)
{
	DataSet * ds = DataSetPackage::pkg()->dataSet();
	Filter  * f  = ds ? ds->shownFilter() : nullptr;

	if(!f)
		return;

	const std::string name = fq(newName);

	//Guard against empty names, renaming the (single, unnamed) default filter, and name collisions:
	//duplicate filter names would make filter(name)/filterGetId lookups ambiguous.
	if(name.empty() || name == DEFAULT_FILTER_NAME || (name != f->name() && !Filter::filterNameIsFree(name, ds)))
		return;

	f->setName(name);
	emit filterChanged();
	emit filterDropDownListChanged();
}

void FilterModel::deleteCurrentFilter()
{
	if(DataSetPackage::pkg()->dataSet())
		DataSetPackage::pkg()->dataSet()->deleteShownFilter();
	emit filterChanged();
	emit filterDropDownListChanged();
}

void FilterModel::addFilter(int dataSetId)
{
	DataSet * dataSet = dataSetId == -1 
			? DataSetPackage::pkg()->dataSet() 
			: DataSetPackage::pkg()->workspace() 
			  ? DataSetPackage::pkg()->workspace()->dataSetById(dataSetId) 
			  : nullptr;
	
	if(dataSet)
		dataSet->addFilter();
}
