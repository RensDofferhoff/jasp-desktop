#ifndef DATASETREGISTRY_H
#define DATASETREGISTRY_H

#include <QObject>
#include <map>
#include <string>

#include "utils.h"
#include "jaspclient/jaspclient.h"

class DataModel;
class DataViewBuffer;
class ViewFiller;

/// NEO dataset registry (refactor_design/data-model-design.md §3.2).
///
/// Thin owner of the open DataModels — datasets are plural from day one, mirroring the
/// orchestrator's dataset index. Knows nothing about columns; tracks which dataset is
/// *active* (the one the single-dataset-era UI currently looks at) and will later route
/// data_changed/data_update payloads by dataset id.
class DatasetRegistry : public QObject
{
	Q_OBJECT
public:
	explicit DatasetRegistry(QObject * parent = nullptr);

	/// Create-or-update the DataModel for the result's dataset id, populate it from the
	/// kind:"data" result payload ({dataset_id, rows, schema}) and make it active.
	/// Returns the model (nullptr when the result carries no dataset id).
	DataModel *			openFromResult(const JaspClient::Result & result, const std::string & sourcePath);

	DataModel *			active() const;									///< the dataset the UI is looking at (nullptr when none open)
	DataModel *			dataset(const std::string & id) const;			///< nullptr when unknown
	const std::string &	activeId() const	{ return _activeId;		}
	size_t				count() const		{ return _models.size();	}
	stringvec			openIds() const;

	void				setActive(const std::string & id);				///< emits activeChanged on change
	void				clear();											///< workspace reset: unbind active, then drop all models

	/// The grid's current viewport row range [firstRow, lastRow) — forwarded to the active
	/// dataset's fill scheduler (sliding mode: urgent viewport-miss fetches + down-biased
	/// background fill around it, format doc §2.5). No-op without an active view.
	void				setViewportRows(uint64_t firstRow, uint64_t lastRow);

	/// The ACTIVE dataset's view buffer (data-view-design §7.6): singular, registry-owned —
	/// created when a dataset becomes active, dropped on switch/close/reset. nullptr when no
	/// dataset is active (or the active one has no rows). GridModel binds to it.
	DataViewBuffer *		viewBuffer() const	{ return _viewBuffer;		}
	ViewFiller *			viewFiller() const	{ return _viewFiller;		}

signals:
	void				datasetOpened(const QString & id);
	void				datasetClosed(const QString & id);
	void				activeChanged(const QString & id);				///< "" when no dataset is active anymore

private:
	void				dropView();				///< stop the filler + drop the buffer (memory policy §7.6)
	void				startView();				///< fresh buffer + filler for the active dataset (rows > 0)

	std::map<std::string, DataModel*>	_models;
	std::string							_activeId;
	DataViewBuffer					*	_viewBuffer	= nullptr;	///< the active dataset's resident cells
	ViewFiller						*	_viewFiller	= nullptr;	///< its chunked fill driver
	uint64_t							_viewEpoch	= 0;		///< bumped per fill — the buffer's fill identity
};

#endif // DATASETREGISTRY_H
