#ifndef DATABASEINTERFACE_H
#define DATABASEINTERFACE_H

#include <sqlite3.h>
#include <string>
#include "columntype.h"
#include "utils.h"
#include <json/json.h>

class DataSet;
class Column;

///Single point of interaction with sqlite, can later be turned into an interface for supporting other sql
class DatabaseInterface
{
public:
	typedef std::function<void(sqlite3_stmt *stmt)> bindParametersType;

				DatabaseInterface(bool create = false);
				~DatabaseInterface();

	static		DatabaseInterface * singleton() { return _singleton; }

	bool		hasConnection() { return _db; }

	void		runQuery(		const std::string & query,		std::function<void(sqlite3_stmt *stmt)>		bindParameters,				std::function<void(size_t row, sqlite3_stmt *stmt)>		processRow);	///< Runs a single query and then goes through the resultrows while calling processRow for each.
	void		runStatements(	const std::string & statements);																																				///< Runs several sql statements without looking at the results.
	int			runStatementsId(const std::string & statements);																																				///< Runs several sql statements only looking for a single returned value from the results.
	void		runStatements(	const std::string & statements, std::function<void(sqlite3_stmt *stmt)>	bindParameters);																						///< Runs several sql statements without looking at the results. Arguments can be set by supplying bindParameters.
	int			runStatementsId(const std::string & statements, std::function<void(sqlite3_stmt *stmt)>	bindParameters);																						///< Runs (several) sql statements and only looks for a single value, this would usually be a id resulting from an insert
	void		runStatements(	const std::string & statements, std::function<void(sqlite3_stmt *stmt)>	bindParameters,	std::function<void(size_t row, sqlite3_stmt *stmt)>	processRow);						///< Runs several sql statements. Arguments can be set by supplying bindParameters and use processRow to read from the results.

	//DataSets
	int			dataSetGetId();
	bool		dataSetExists(		int dataSetId);
	void		dataSetDelete(		int dataSetId);
	int			dataSetInsert(						const std::string & dataFilePath = "", const std::string & databaseJson = "", const std::string & emptyValuesJson = "");	///< Inserts a new DataSet row into DataSets and creates an empty DataSet_#id. returns id
	void		dataSetUpdate(		int dataSetId,	const std::string & dataFilePath = "", const std::string & databaseJson = "", const std::string & emptyValuesJson = "");	///< Updates an existing DataSet row in DataSets
	void		dataSetLoad(		int dataSetId,		  std::string & dataFilePath,			 std::string & databaseJson,			std::string & emptyValuesJson, int & revision);			///< Loads an existing DataSet row into arguments
	static int	dataSetColCount(	int dataSetId);
	static int	dataSetRowCount(	int dataSetId);
	void		dataSetSetRowCount(	int dataSetId, size_t rowCount);
	std::string dataSetName(		int dataSetId) const;
	int			dataSetIncRevision(	int dataSetId);
	int			dataSetGetRevision(	int dataSetId);
	int			dataSetGetFilter(	int dataSetId);

	void		dataSetBatchedValuesUpdate(DataSet * data);

	//Filters
	std::string filterName(				int filterIndex) const;
	int			filterGetId(			int dataSetId);
	bool		filterSelect(			int filterIndex,			boolvec & bools);																	///< Loads result and errorMsg and returns whether there was a change in either of those.
	void		filterWrite(			int filterIndex,	const	boolvec & values);																	///< Overwrites the current filter values, no checks are done on the size. If too few the rest is TRUE nd superfluous bools are ignored.
	int			filterInsert(			int dataSetId,		const std::string & rFilter = "", const std::string & generatedFilter = "", const std::string & constructorJson = "", const std::string & constructorR = "");		///< Inserts a new Filter row into Filters and creates an empty FilterValues_#id. It returns id
	void		filterUpdate(			int filterIndex,	const std::string & rFilter = "", const std::string & generatedFilter = "", const std::string & constructorJson = "", const std::string & constructorR = "");		///< Updates an existing Filter row in Filters
	void		filterLoad(				int filterIndex,		  std::string & rFilter,			std::string & generatedFilter,			  std::string & constructorJson,			std::string & constructorR, int & revision);			///< Loads an existing Filter row into arguments
	void		filterClear(			int filterIndex);																					///< Clears all values in Filter
	void		filterDelete(			int filterIndex);
	int			filterGetDataSetId(		int filterIndex);
	std::string	filterLoadErrorMsg(		int filterIndex);
	void		filterUpdateErrorMsg(	int filterIndex, const	std::string & errorMsg);
	int			filterIncRevision(		int filterIndex);
	int			filterGetRevision(		int filterIndex);

	//Columns & Data/Values
	//Index stuff:
	int			columnInsert(			int dataSetId, int index = -1, const std::string & name = "", columnType colType = columnType::unknown);	///< Insert a row into Columns and create the corresponding columns in DataSet_? Also makes sure the indices are correct
	int			columnLastFreeIndex(	int dataSetId);
	void		columnIndexIncrements(	int dataSetId, int index);																			///< If index already is in use that column and all after are incremented by 1
	void		columnIndexDecrements(	int dataSetId, int index);																			///< Indices bigger than index are decremented, assumption is that the previous one using it has been removed already
	int			columnIdForIndex(		int dataSetId, int index);
	int			columnIndexForId(		int columnId);
	void		columnSetIndex(			int columnId, int index);		///< If this is used by JASP and changes the index the assumption is all will be brought in order. By setting the indices correct for all columns.
	int			columnIncRevision(		int columnId);
	int			columnGetRevision(		int columnId);

	//id stuff:
	int			columnGetDataSetId(			int columnId);
	void		columnDelete(				int columnId);			///< Also makes sure indices stay as contiguous and correct as before.
	void		columnSetType(				int columnId, columnType colType);
	void		columnSetInvalidated(		int columnId, bool invalidated);
	void		columnSetName(				int columnId, const std::string & name);
	void		columnGetBasicInfo(			int columnId,		std::string & name, columnType & colType, int & revision);
	void		columnSetComputedInfo(		int columnId, bool   invalidated, computedColumnType   codeType, const	std::string & rCode, const	std::string & error, const	std::string & constructorJson);
	bool		columnGetComputedInfo(		int columnId, bool & invalidated, computedColumnType & codeType,		std::string & rCode,		std::string & error,		Json::Value & constructorJson);
	void		columnSetValues(			int columnId, const intvec	  & ints);
	void		columnSetValues(			int columnId, const doublevec & dbls);
	void		columnSetValue(				int columnId, size_t row, int value);
	void		columnSetValue(				int columnId, size_t row, double value);
	intvec		columnGetLabelIds(			int columnId);
	size_t		columnGetLabelCount(		int columnId);
	void		columnGetValuesInts(		int columnId,	intvec		& ints);
	void		columnGetValuesDbls(		int columnId,	doublevec	& dbls);
	std::string columnBaseName(				int columnId) const;
	void		dataSetBatchedValuesLoad(	DataSet * data);


	//Labels
	void		labelsClear(			int columnId);
	int			labelAdd(				int columnId,	int value, const std::string & label, bool filterAllows, const	std::string & description = "", const	std::string & originalValueJson = "");
	void		labelSet(		int id,	int columnId,	int value, const std::string & label, bool filterAllows, const	std::string & description = "", const	std::string & originalValueJson = "");
	void		labelDelete(	int id);
	void		labelLoad(		int id,	int & columnId,	int & value,	 std::string & label, bool & filterAllows,		std::string & description,				std::string & originalValueJson,	int & order);
	void		labelSetOrder(	int id, int order);
	void		labelsLoad(		Column * column);
	void		labelsWrite(	Column * column);
	void		labelsSetOrder(	const intintmap & orderPerDbId);

	//Transactions
	void		transactionWriteBegin();						///< runs BEGIN EXCLUSIVE and waits for sqlite to not be busy anymore if some other process is writing. Tracks whether nested and only does BEGIN+COMMIT at lowest depth
	void		transactionWriteEnd(bool rollback = false);		///< runs COMMIT or ROLLBACK based on rollback and ends the transaction.  Tracks whether nested and only does BEGIN+COMMIT at lowest depth
	void		transactionReadBegin();							///< runs BEGIN DEFERRED and waits for sqlite to not be busy anymore if some other process is writing  Tracks whether nested and only does BEGIN+COMMIT at lowest depth
	void		transactionReadEnd();							///< runs COMMIT and ends the transaction. Tracks whether nested and only does BEGIN+COMMIT at lowest depth

	private:
	void		_runStatements(				const std::string & statements,						std::function<void(sqlite3_stmt *stmt)> *	bindParameters = nullptr,	std::function<void(size_t row, sqlite3_stmt *stmt)> *	processRow = nullptr);	///< Runs several sql statements without looking at the results. Unless processRow is not NULL, then this is called for each row.
	void		_runStatementsRepeatedly(	const std::string & statements, std::function<bool(	std::function<void(sqlite3_stmt *stmt)> **	bindParameters, size_t row)> bindParameterFactory, std::function<void(size_t row, size_t repetition, sqlite3_stmt *stmt)> * processRow = nullptr);

	std::string dbFile() const;									///< Convenience function for getting the filename where sqlite db should be
	void		create();										///< Creates a new sqlite database in sessiondir and loads it
	void		load();											///< Loads a sqlite database from sessiondir (after loading a jaspfile)
	void		close();										///< Closes the loaded database and disconnects

	int			_transactionWriteDepth	= 0,
				_transactionReadDepth	= 0;

	sqlite3	*	_db = nullptr;

	static			std::string _wrap_sqlite3_column_text(sqlite3_stmt * stmt, int iCol);
	static const	std::string _dbConstructionSql;

	static DatabaseInterface * _singleton;

	friend class DataSetPackage;
};

#endif // DATABASEINTERFACE_H
