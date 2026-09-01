#include "workspacemodel.h"
#include "datasetpackage.h"
#include "qutils.h"
#include "gui/preferencesmodel.h"
#include "log.h"

WorkspaceModel* WorkspaceModel::_singleton = nullptr;

WorkspaceModel::WorkspaceModel(QObject *parent)
	: QObject(parent)
{
	if(_singleton) throw std::runtime_error("WorkspaceModel can be constructed only once!");

	_singleton = this;

	connect(DataSetPackage::pkg(),	&DataSetPackage::loadedChanged,					this,	&WorkspaceModel::refresh				);
	connect(DataSetPackage::pkg(),	&DataSetPackage::shownDataSetChanged,			this,	&WorkspaceModel::refresh				);
	connect(DataSetPackage::pkg(),	&DataSetPackage::nameChanged,					this,	&WorkspaceModel::nameChanged			);
	connect(DataSetPackage::pkg(),	&DataSetPackage::descriptionChanged,			this,	&WorkspaceModel::descriptionChanged		);
	connect(DataSetPackage::pkg(),	&DataSetPackage::workspaceEmptyValuesChanged,	this,	&WorkspaceModel::emptyValuesChanged		);
}

void WorkspaceModel::refresh()
{
	emit nameChanged();
	emit descriptionChanged();
	emit emptyValuesChanged();
}

QStringList WorkspaceModel::emptyValues() const
{
	DataSet * set = DataSetPackage::pkg()->workspace() ? DataSetPackage::pkg()->workspace()->shownDataSet() : nullptr;
	return tql(set ? set->emptyValuesAsStrings() : stringset());
}

QString WorkspaceModel::name() const
{
	return DataSetPackage::pkg()->name();
}

QString WorkspaceModel::description() const
{
	return DataSetPackage::pkg()->description();
}

void WorkspaceModel::setDescription(const QString &desc)
{
	Q_UNUSED(desc);
	if (desc == description()) return;
	if(!DataSetPackage::pkg()->dataSet()) return;

	// The excision, Cut 4: the workspace-property undo command is gone with the legacy
	// data route; description has no lane wire support either (returns as jasp:description
	// metadata with the labels-editor era).
	Log::log() << "WorkspaceModel::setDescription: description is not yet on the lane wire — ignored" << std::endl;
}

void WorkspaceModel::removeEmptyValue(const QString &value)
{
	Q_UNUSED(value);
	if(!DataSetPackage::pkg()->dataSet()) return;

	// The excision, Cut 4: empty values are a legacy loading concept (which strings read
	// as empty); on NEO the lane backend owns the data. Returns with a NEO-era design.
	Log::log() << "WorkspaceModel::removeEmptyValue: empty-value editing is legacy — ignored" << std::endl;
}

void WorkspaceModel::addEmptyValue(const QString &value)
{
	Q_UNUSED(value);
	if(!DataSetPackage::pkg()->dataSet()) return;

	Log::log() << "WorkspaceModel::addEmptyValue: empty-value editing is legacy — ignored" << std::endl;
}

void WorkspaceModel::resetEmptyValues()
{
	if(!DataSetPackage::pkg()->dataSet() || !PreferencesModel::prefs()) return;

	Log::log() << "WorkspaceModel::resetEmptyValues: empty-value editing is legacy — ignored" << std::endl;
}
