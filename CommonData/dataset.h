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
#ifndef DATASET_H
#define DATASET_H

#include "databaseconnectioninfo.h"
#include "datasetbasenode.h"
#include "emptyvalues.h"
#include "filter.h"
#include "version.h"
#include "columnencoder.h"
#include "columninfo.h"
// The legacy DataSetSyncer is REMOVED (the excision, Cut 1): sync returns as a BACKEND
// feature — the pinned policy (P14) lives in refactor_design/HANDOVER-excision.md.
#include "qutils.h"
#include <unordered_map>

class Workspace;
class UndoStack;

class DataSet : public DataSetBaseNode
{
	Q_OBJECT
	
	//Would be nice to have EmptyValuesQ also and make it available as a property here
	Q_PROPERTY(QString				description					READ descriptionQ				WRITE setDescriptionQ			NOTIFY descriptionChanged				)
	Q_PROPERTY(QString				dataFile					READ dataFileQ					WRITE setDataFileQ				NOTIFY dataFileChanged					)
	//Q_PROPERTY(QJsonValue			databaseJson				READ databaseJsonQ				WRITE setDatabaseJsonQ			NOTIFY databaseJsonChanged				)
	Q_PROPERTY(bool					dataFileSynch				READ dataFileSynch				WRITE setDataFileSynch			NOTIFY dataFileSynchChanged				)
	Q_PROPERTY(long					dataFileTimestamp			READ dataFileTimestamp			WRITE setDataTimestamp			NOTIFY dataTimestampChanged				)
	Q_PROPERTY(int					columnsLabelFilteredCount	READ columnsLabelFilteredCount											NOTIFY columnsLabelFilteredCountChanged	)
	Q_PROPERTY(Filter	*			shownFilter					READ shownFilter													NOTIFY shownFilterChanged				)
	Q_PROPERTY(QString				name							READ name															CONSTANT										)
	Q_PROPERTY(QString				title						READ title						WRITE setTitle					NOTIFY titleChanged						)
	Q_PROPERTY(QString				rCode						READ rCodeQ						WRITE setRCodeQ					NOTIFY rCodeChanged						)
	Q_PROPERTY(computedColumnType	codeType					READ codeType					WRITE setCodeType				NOTIFY codeTypeChanged					)
	Q_PROPERTY(bool					invalidated					READ invalidated												NOTIFY invalidatedChanged				)
	Q_PROPERTY(QString				error						READ errorQ						WRITE setErrorQ					NOTIFY errorChanged						)
	Q_PROPERTY(int					defaultInputFilterId		READ defaultInputFilterId		WRITE setDefaultInputFilterId	NOTIFY defaultInputFilterChanged		)
	// Emit signals also in refresh
	
	friend class Filter;

public:
	typedef 	std::map<std::string,columnType>	colTypeMap;
	// The excision, Cut 3: DatabaseInterface is gone — the DBIF/DBCIF aliases died with it.
	
							DataSet(Workspace * workspace); ///< a fresh dataset: id minted from the process-global counter
							~DataSet();
	
			Workspace	*	workspace()			const		{ return	_workspace; }
			Filter		*	defaultFilter()		const 		{ return	_defaultFilter;	}
			Filter		*	shownFilter()		const 		{ return	!_shownFilter ? _defaultFilter : _shownFilter;	}
	const	Filters		&	filters()			const		{ return	_filters;		}
			void			deleteShownFilter();
			void			addFilter();
			void			showFilter(Filter * filter);
			Filter *		showFilter(const std::string & filterName);
			Filter *		showFilter(const QString & filterName);
			// The excision, Cut 5: the Columns container and every Column* accessor died with
			// Column — the wire schema (`schema()` / `schemaColumn*`) is THE column truth.
	const	EmptyValues *	emptyValues()       const		{ return	_emptyValues; }
			EmptyValues *	emptyValues()						{ return	_emptyValues; }
			QString			name()				const;
			QString			title()				const;

			int				id()					const { return _dataSetId;				}

	// ————— NEO lane identity + wire schema (multi-dataset fold; data-model-design.md §3.2) —————
	// One id space: the orchestrator-assigned dataset id is THE id (their SQLite int stays for
	// db rows only). "" = not orchestrator-backed (legacy import / computed) — serve via the legacy path.
	const	std::string	&	datasetId()				const	{ return _laneDatasetId;				}
				bool				isOpen()		const		{ return !_laneDatasetId.empty();	}
				uint64_t			schemaRows()			const	{ return _schemaRows;					}
				uint64_t			laneRevision()			const	{ return _laneRevision;					}
	const	std::vector<ColumnInfo> &	schema()	const	{ return _schemaColumns;				}
	const	ColumnInfo	*	schemaColumnAt(size_t index)				const;	///< nullptr when out of range
	const	ColumnInfo	*	schemaColumn(const std::string & name)	const;	///< nullptr when absent
	int					schemaColumnIndex(const std::string & name)	const;	///< -1 when absent
	/// Populate from the orchestrator's kind:"data" terminal result and mirror the metadata
	/// (columns + row count; NEVER row data) into this DataSet so the per-dataset provider
	/// chain (filters, forms, headers) serves lane metadata. Emits schemaChanged. A fresh
	/// identity starts a fresh revision space (0).
	void				applySchema(const std::string & datasetId, uint64_t rows, const Json::Value & schema, const std::string & sourcePath);
	/// Land a `data_changed` push (data-edit-design §6, Increment 4): adopt the NEW revision
	/// (idempotent — `revision ≤ current` is ignored, the §6 ordering rule), the new row total,
	/// and the post-edit schema IFF one was carried, then refresh the mirror and emit
	/// schemaChanged — which restarts the view lane at the new revision (the v1 whole-buffer
	/// drop; the invalidation descriptor makes range-aware invalidation a drop-in later,
	/// §11 open item 2). Only a lane-bound dataset takes revisions.
	///
	/// NB — external changes (backend sync, not yet built; DataSetSyncer's header has the
	/// pinned policy): a cause:"external" push lands HERE and means a FRESH RELOAD — nothing
	/// survives: no edit rebase, and _undoStack must be cleared (external revision bumps fork
	/// history; every stored inverse blob is then base_revision < current — not stale-refused,
	/// but meaningless against the reloaded data). Candidate later exception: the labels
	/// overlay (value-keyed; reattaches to surviving values).
	void				applyRevision(uint64_t revision, uint64_t rows, bool hasRows, const Json::Value & schema, const Json::Value & invalidation);
			bool			dataFileSynch()			const { return _dataFileSynch;			}
			
	const	std::string &	dataFilePath()			const { return _dataFilePath;			}
			// The excision, Cut 5: dataFileCanHaveLabels died with the label editor guts (B2).
			qint64			dataFileTimestamp()		const { return _dataFileTimestamp;		}
	const	Json::Value &	databaseJson()			const { return _database;				}
			bool			writeBatchedToDB()		const { return _writeBatchedToDBDepth;		}
			bool			filterExists(const std::string & name) { return filter(name); }
			Filter *		filter(const std::string & name);
			Filter *		filter(int id);
			
			int				rowCount(		const QModelIndex &parent = QModelIndex())										const	override;
			int				columnCount(	const QModelIndex &parent = QModelIndex())										const	override;
			QVariant		data(			const QModelIndex &index, int role = Qt::DisplayRole)							const	override;
			bool			setData(		const QModelIndex &index, const QVariant &value, int role)								override;
			QVariant		headerData(		int section, Qt::Orientation orientation, int role = Qt::DisplayRole )			const	override;
			Qt::ItemFlags	flags(			const QModelIndex &index)														const	override;
			
			bool			insertRows(		int row,		int count, const QModelIndex & aparent = QModelIndex())						override;
			bool			insertColumns(	int column,		int count, const QModelIndex & aparent = QModelIndex())						override;
			bool			removeRows(		int row,		int count, const QModelIndex & aparent = QModelIndex())						override;
			bool			removeColumns(	int column,		int count, const QModelIndex & aparent = QModelIndex())						override;
			// The excision, Cut 5: these were the legacy TableModel write ops (Column-value
			// surgery); they become inert — structural change on lane is remote (revisions).

			// The excision, Cut 3: dbCreate/dbUpdate/dbLoad died with DatabaseInterface; dbDelete
			// survives as the PURELY IN-MEMORY teardown (no sqlite rows exist). Ids are minted from
			// the process-global counter in the ctor; setters keep bumping the legacy revision
			// (incRevision) so behaviour is unchanged minus the sqlite writes.
			int				columnsLabelFilteredCount()	const;
			void			dbDelete();
			void			beginBatchedToDB();
			void			endBatchedToDB(std::function<void(float)> progressCallback = [](float){});
			
			// The excision, Cut 5: the legacy column-write API (removeColumn/insertColumn(s)/
			// createColumn/createComputedColumn/columnsReorder/columnRefreshed/columnsSet·Reverse·
			// Apply·LabelFilter etc.) died with Column — write gestures are DataEditCommands on
			// the rail.
			ColumnEncoder	&	encoder()			{ return *_encoder; }
	const	ColumnEncoder	&	encoder()		const { return *_encoder; }
			int				getColumnIndex(	const	std::string &	name) const;
			
			bool			isColumnNameFree(const std::string & name)		const;
			
			QString			dataFileQ()			const;
			QString			descriptionQ()		const;
			long			dataTimestamp()		const;
			bool			isDatabase()						const	{ return _database != Json::nullValue;				}
			
			void			setTitle(				const QString & title);
			void			setDataFileQ(			const QString &	newDataFile);
			void			setDescriptionQ(		const QString &	newDescription);
			//void			setDatabaseJsonQ(		const QString &	newDatabaseJson);
			void			setDataFileAndTimeStamp(const std::string &dataFilePath, long timestamp);
			
			void			resetAllFilters();
			void			resetFilterCounters();
			// The excision, Cut 5: resetVariableTypes re-guessed types from legacy Column values —
			// the lane owns typing (schema_change ops); returns with backend sync (P14 territory).
			
			stringvec		getColumnNames();
			colTypeMap		getColumnTypesMap();
			void			setupEncoderPrefix();
			

			void			setDataFile(		const std::string & dataFilePath);
			void			setDataTimestamp(	long timestamp);
			void			setDatabaseJson(	const Json::Value & databaseJson);
			void		setDataFileSynch(	bool	synchronizing);
			bool			synchingData()		const { return _synchingDataNow; }
			

			void			setDataFile( const std::string & dataFilePath, long timestamp)	{ _dataFilePath	= dataFilePath;	_dataFileTimestamp = timestamp; incRevision(); }	// was dbUpdate()
			void			setDatabaseJson(	const std::string & databaseJson)	{ Json::Reader().parse(databaseJson, _database); incRevision(); }	// was dbUpdate()
			char			csvDelimiter()		const									{ return _csvDelimiter; }
			void			setCsvDelimiter(	char delimiter)						{ _csvDelimiter		= delimiter;			incRevision(); }	// was dbUpdate()

			void			setRowCountMetadata(size_t rowCount);

			void			incRevision() override;
			bool			checkForUpdates(std::function<void(float)> progressCallback = [](float){});
			void			runComputedDataset(QString code, int defaultInputFilterId);
			// The excision, Cut 5: runComputedColumn + computedColumns() died with Column;
			// computed COLUMNS return as derivations, computed DATASETS stay (below).

			//Computed-dataset state (a whole DataSet generated from R code), mirroring the per-column state.
			bool					isComputed()				const	{ return _codeType != computedColumnType::notComputed;									}
			bool					isComputedRCode()			const	{ return _codeType == computedColumnType::rCode;								}
			computedColumnType		codeType()					const	{ return _codeType;															}
			bool					invalidated()				const	{ return _invalidated;														}
			void					invalidate()						{ setInvalidated(true);														}
			void					validate()							{ setInvalidated(false);													}
			std::string				rCode()						const	{ return _rCode;															}
			QString					rCodeQ()					const	{ return tq(_rCode);														}
			std::string				rCodeStripped()				const;
			QString					errorQ()					const	{ return tq(_error);														}
			std::string				error()						const	{ return _error;															}
			int						defaultInputFilterId()		const	{ return _defaultInputFilterId;											}
			Filter				*	defaultInputFilter()		const;
			DataSet				*	defaultInputDataSet()		const;
			bool					setRCode(				const std::string	& rCode);
			bool					setRCodeQ(				const QString		& rCode)	{ return setRCode(fq(rCode));						}
			void					setCodeType(			computedColumnType codeType);
			void					setInvalidated(			bool invalidated);
			bool					setError(				const std::string	& error);
			bool					setErrorQ(				const QString		& error)				{ return setError(fq(error));			}
			bool					setDefaultInputFilterId(int defaultInputFilterId);
			bool					tryAndRunComputedDataset();
			bool					iShouldBeSentAgain();
			void					checkForDependentDatasetsToBeSent(bool refreshMe = false);
			void					dbUpdateComputedDatasetStuff();
			
			void			loadOldComputedColumnsJson(const Json::Value & json);	///< The excision, Cut 5: inert — legacy computed-Column restore is gone.
			stringset		findUsedColumnNames(std::string searchThis);

	// The excision, Cut 3: DBIF & db() died with DatabaseInterface.
	
			void			setEmptyValuesJson(			const Json::Value & emptyValues, bool updateDB = true);
			
	const	std::string	&	description()																	const	{ return _description; }
	const	stringset	&	emptyValuesAsStrings()															const	{ return _emptyValues->emptyStrings();		}
			void			setEmptyValuesFromStrings(const stringset& values);
			void			setDescription(				const std::string& desc);
			// The excision, Cut 5: jsonForCompare hashed legacy Column values — the wire schema
			// + revision is the comparison currency now.
			// writeToOStream died with the exporters (the excision, Cut 2); export returns as a
			// lane conversion in a later NEO era.

signals:
			void			schemaChanged();	///< applySchema landed (id/rows/schema ready or refreshed)
			void			manualEditMade(); 
			void			datasetChanged(				int						dataSetId,
															QStringList				changedColumns,
															QStringList				missingColumns,
															QMap<QString, QString>	changeNameColumns,
															bool					rowCountChanged,
															bool					hasNewColumns); 
			void			labelsReordered(			QString columnName);
			// The excision, Cut 5: labelFilterChanged + labelChanged(Column*) died with the label
			// editor guts (B2 rebuilds on the jasp:labels overlay).
			
			void			allFiltersReset();
			void			showWarning(						QString title, QString msg);
			void			descriptionChanged();
			void			titleChanged();
			void			dataFileChanged();
			void			databaseJsonChanged();
			void			dataFileSynchChanged();
			void			dataTimestampChanged();
			void			columnsLabelFilteredCountChanged();
			void			refreshAllAnalyses(Filter * f);
			void			refreshAllCompCols(Filter * f);
			void			synchingIntervalPassed();
			void			columnTypeChanged(QString name);	///< still emitted on schema type changes (lane)
			void			sendFilter(int dataSetID, const QString & generatedFilter, const QString & filter);
			void			sendFilterByName(int dataSetID, const QString & name, const QString & module = "*");
			void			filtersCountChanged();
			void			shownFilterChanged(DataSet * data);
			void			filterRemoved(Filter * f);
			void			askPassword(	QString title, QString message);
			bool			showYesNo(		QString title, QString message);
			void			emptyValuesChanged();
			void			rCodeChanged();
			void			codeTypeChanged();
			void			invalidatedChanged();
			void			errorChanged();
			void			defaultInputFilterChanged();
			
public slots:
			void			refresh(bool doColumnsToo = true);
			void			runFilters();
			void			filterByNameDone(int dataSetID, const QString & name, const QString & error);
			

public:
			Filter		*	createFilter(const std::string & name, bool createIfMissing = true) { return new Filter(this, name, createIfMissing); }
			void			registerFilter(Filter * f);
			void			removeFilter(Filter * f);
			
private:
			void			upgradeEmptyValsFrom018To019(const Json::Value & emptyVals);
			void			setEmptyValuesJsonOldStuff(	const Json::Value & emptyValues);
			// The excision, Cut 5: the columnsApply family + loadOldComputedColumnsJson died with
			// Column.

			
private slots:
			void			handleDataSetChanged(	int						dataSetID,
													QStringList				changedColumns,
													QStringList				missingColumns,
													QMap<QString, QString>	changeNameColumns,
													bool					rowCountChanged,
													bool					hasNewColumns);

public:
	static QVariant			getDataSetViewLines(bool up, bool left, bool down, bool right)								    ;
	
	
	UndoStack		*	undoStack()				const	{ return _undoStack; }
	// The excision, Cut 5: insertColumnSpecial + pasteSpreadsheet (the legacy column-write
	// surface) died with Column.
	
protected:
	// The excision, Cut 6: getRowFilter died with the default filter's per-row mask.
	
private:
	Workspace			*	_workspace				= nullptr;
	ColumnEncoder		*	_encoder				= nullptr;
	Filter			*	_defaultFilter			= nullptr,
					*	_shownFilter			= nullptr;
	Filters					_filters;
	EmptyValues			*	_emptyValues			= nullptr;
	int						_dataSetId				= -1,
							_rowCount				= -1,
							_writeBatchedToDBDepth	= 0;
	long					_dataFileTimestamp		= 0;			// The excision, Cut 5: _columns/_shownColumn/_changedDuringBatch (ColumnSet) died with Column.
	std::string				_dataFilePath,
							_title;
	bool					_dataFileSynch			= false,
							_synchingDataNow		= false;
	char					_csvDelimiter			= '\0';
	Json::Value				_database				= Json::nullValue;

	// NEO lane identity + wire schema (multi-dataset fold) — see the public block above
	std::string				_laneDatasetId;
	uint64_t				_schemaRows				= 0;
	uint64_t				_laneRevision			= 0;	///< the §6 staleness stamp — received, compared, echoed; never reasoned about
	std::vector<ColumnInfo>	_schemaColumns;
	std::unordered_map<std::string, size_t>	_schemaColumnIndex;	///< first-wins, same semantics as the old DataModel linear scan
	/// Shared by applySchema (open) and applyRevision (data_changed): rebuild `_schemaColumns`
	/// from a wire schema array, grow the legacy-column mirror, land the row count, emit
	/// schemaChanged. `schema` must be a non-empty array (callers guard).
	void				landWireSchema(const std::string & datasetId, uint64_t rows, const Json::Value & schema);
	static stringset		_defaultEmptyvalues;	// Default empty values if workspace do not have its own empty values (used for backward compatibility)
	std::string				_description;
	UndoStack			*	_undoStack				= nullptr;
	computedColumnType		_codeType				= computedColumnType::notComputed;
	bool					_invalidated			= false;
	int						_defaultInputFilterId	= -1;
	std::string				_rCode,
							_error;
};

typedef std::vector<DataSet*> DataSets;

#endif // DATASET_H
