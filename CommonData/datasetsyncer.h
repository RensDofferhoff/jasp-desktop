#ifndef DATASETSYNCER_H
#define DATASETSYNCER_H

#include <QObject>
#include <QFileSystemWatcher>
#include <json/json.h>

#include "databaseconnectioninfo.h"

class DataSet;

/// Legacy (C++ frontend) data synchronization: watches the source file / polls the source
/// database and re-imports through the legacy Importer into legacy Column storage. Lane
/// datasets never use it (the R1 gate: `dataSet->isOpen()` skips the starts) — for
/// orchestrator-backed datasets sync will be BACKEND-owned instead: the orchestrator
/// watches the source, reconverts through the data_open machinery, bumps the revision and
/// broadcasts data_changed (cause: external).
///
/// Policy pinned 2026-08-31 for that backend sync — an external change is a FRESH RELOAD:
/// NOTHING survives. No rebase of local edits onto the new source, and the dataset's undo
/// stack is CLEARED (dropped, not refused — see DataSet::applyRevision: an external revision
/// bump forks history, so stored inverse blobs are strict-LIFO-void and meaningless against
/// the reloaded data). Candidate LATER exception, only if wanted: the labels overlay
/// (jasp:labels, P11) — value-keyed and order-independent, it would reattach to whichever
/// values survive the reload and cannot conflict by construction.
class DataSetSyncer : public QObject
{
	Q_OBJECT
public:
	DataSetSyncer(DataSet * dataSet, QObject * parent = nullptr);
	~DataSetSyncer();

	void					startFileSyncing(const QString & filePath);
	void					stopFileSyncing(bool isExit = false);
	bool					isFileSyncing()								const { return _fileWatcher && !_fileWatcher->files().isEmpty(); }

	void					startDatabaseSyncing(const Json::Value & dbJson, bool syncImmediately = false);
	void					stopDatabaseSyncing(bool isExit = false);
	bool					isDatabaseSyncing()							const { return _dbInfo && _dbInfo->synching(); }
	const Json::Value &		databaseJson()								const { return _databaseJson; }

	void					syncNow();
	/// Called by the file/database IO machinery once a sync actually completes, so that
	/// syncingFinished(int,bool) reflects the real end of the sync (not the request launch).
	void					setSyncingResult(bool success);

signals:
	void					syncingStarted(int dataSetId);
	void					syncingFinished(int dataSetId, bool success);

	void					askUserForRelink(int dataSetId);
	QString					askPassword(int dataSetId, QString title, QString message);
	bool					askYesNo(int dataSetId, QString title, QString message);
	void					showWarning(int dataSetId, QString title, QString message);

	void					syncRequired(int dataSetId, DataSet * dataSet, QString locator, QString extension, QString databaseJson);

private slots:
	void					fileChanged(const QString & path);
	void					databaseSyncIntervalPassed();

private:
	void					doSync();

	DataSet				*	_dataSet			= nullptr;
	QFileSystemWatcher	*	_fileWatcher		= nullptr;
	DatabaseConnectionInfo	*	_dbInfo			= nullptr;
	Json::Value				_databaseJson		= Json::nullValue;
	bool					_isSyncing			= false;
	bool					_isPendingFileSync	= false; //a file change arrived while a sync was in flight
};

#endif // DATASETSYNCER_H
