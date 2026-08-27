#include "datasetregistry.h"
#include "datamodel.h"
#include "dataviewbuffer.h"
#include "viewfiller.h"
#include "utilities/qutils.h"
#include "log.h"

DatasetRegistry::DatasetRegistry(QObject * parent)
	: QObject(parent)
{
}

DataModel * DatasetRegistry::openFromResult(const JaspClient::Result & result, const std::string & sourcePath)
{
	if (result.datasetId.empty())
		return nullptr;

	DataModel * model = dataset(result.datasetId);
	const bool isNew = !model;

	if (isNew)
	{
		model = new DataModel(this);
		_models[result.datasetId] = model;
	}

	model->applySchema(result.datasetId, result.rows, result.schema, sourcePath);

	if (isNew)
	{
		Log::log() << "DatasetRegistry: dataset " << result.datasetId << " opened (" << result.rows << " rows, " << model->columnCount() << " columns)" << std::endl;
		emit datasetOpened(tq(result.datasetId));
	}

	setActive(result.datasetId);
	return model;
}

DataModel * DatasetRegistry::active() const
{
	return dataset(_activeId);
}

DataModel * DatasetRegistry::dataset(const std::string & id) const
{
	auto found = _models.find(id);
	return found == _models.end() ? nullptr : found->second;
}

stringvec DatasetRegistry::openIds() const
{
	stringvec ids;
	ids.reserve(_models.size());
	for (const auto & [id, model] : _models)
		ids.push_back(id);
	return ids;
}

void DatasetRegistry::setActive(const std::string & id)
{
	if (_activeId == id)
		return;

	// Memory policy (data-view-design §7.6): only the ACTIVE dataset has a view buffer —
	// drop it BEFORE switching so the ceiling is 1 × budget regardless of open datasets.
	dropView();

	_activeId = id;
	startView();

	emit activeChanged(tq(id));
}

void DatasetRegistry::setViewportRows(uint64_t firstRow, uint64_t lastRow)
{
	// Sliding mode (format doc §2.5): the viewport drives the active dataset's fill scheduler.
	if (_viewFiller)
		_viewFiller->setViewport(firstRow, lastRow);
}

void DatasetRegistry::startView()
{
	DataModel * model = active();
	if (!model || model->rows() == 0)
		return;		// nothing to view (or a schema-only dataset) — the grid shows an empty model

	_viewBuffer = new DataViewBuffer(this);
	_viewBuffer->reset(model->rows(), 0 /* dataset revision — edits bump it in a later increment */, ++_viewEpoch);
	_viewFiller = new ViewFiller(model, _viewBuffer, this);
	_viewFiller->start();	// the fill loop IS the prefetch — back-to-back chunks, background
}

void DatasetRegistry::dropView()
{
	if (_viewFiller)
	{
		_viewFiller->stop();		// aborts any in-flight chunk — its handler slot is dropped
		_viewFiller->deleteLater();
		_viewFiller = nullptr;
	}
	if (_viewBuffer)
	{
		_viewBuffer->deleteLater();
		_viewBuffer = nullptr;
	}
}

void DatasetRegistry::clear()
{
	if (_models.empty() && _activeId.empty())
		return;

	dropView();	// the view belongs to the active dataset — it goes first

	// Unbind the active one FIRST — consumers (ColumnsModel etc.) disconnect while the
	// models are still alive — then drop everything.
	const bool hadActive = !_activeId.empty();
	_activeId.clear();
	if (hadActive)
		emit activeChanged(QString());

	for (auto & [id, model] : _models)
	{
		emit datasetClosed(tq(id));
		model->deleteLater();
	}
	_models.clear();
}
