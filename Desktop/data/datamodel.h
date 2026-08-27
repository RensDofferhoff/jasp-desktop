#ifndef DATAMODEL_H
#define DATAMODEL_H

#include <QObject>
#include <json/value.h>
#include <unordered_map>

#include "utils.h"
#include "columntype.h"

/// NEO data model of ONE dataset (refactor_design/data-model-design.md §3.1).
///
/// Schema + bookkeeping only: the frontend owns no cell values under NEO — the bytes live in
/// the orchestrator's Arrow cache, analyses read them in R runners, and the UI will fetch
/// windowed views via `data_view` (Phase C). This class is what the old DataSet/Column/Label
/// SQLite stack reduces to once values and engine-sync are gone.
///
/// Main-thread only. No DatabaseInterface, no DataSetBaseNode tree, no revision counters.
/// Instances are owned by the DatasetRegistry — never construct one standalone as "the"
/// dataset: datasets are plural.
struct ColumnInfo
{
	std::string		name,			///< canonical column name (what analyses/forms use)
					displayName,	///< real (user) name from the wire (`display_name`)
					description;
	columnType		type = columnType::unknown;		///< scale | ordinal | nominal (never nominalText)
	stringvec		levels;							///< categoricals: dictionary values = R factor levels
	bool			allInteger = false;				///< scale display hint (render 7 not 7.0)
	uint64_t		valueCount = 0;					///< non-empty (non-missing) cells; ZERO IS A VALUE (from the lane; design doc §2)
	uint64_t		distinctCount = 0;				///< distinct values — categoricals exact, scale exact up to the lane cap ~1k (design doc §2); drives all form level/numeric thresholds
	int				numericLevels = 0;				///< categoricals only: distinct NUMERIC levels, computed locale-aware by the lane (design doc §2); scale uses distinctCount

	// computed-column bookkeeping (filled as that machinery is re-attached, Phases B/C)
	int					analysisId = -1;
	computedColumnType	codeType = computedColumnType::notComputed;
	std::string			rCode, constructorJson, error;
	bool				invalidated = false;
	stringset			dependsOn;
};

class DataModel : public QObject
{
	Q_OBJECT
public:
	explicit DataModel(QObject * parent = nullptr);

	// identity
	const std::string &	datasetId()	const	{ return _datasetId;	}
	uint64_t			rows()		const	{ return _rows;			}
	const std::string &	sourcePath()const	{ return _sourcePath;	}
	bool				isOpen()	const	{ return !_datasetId.empty();	}

	/// Single writer path: populate/replace the schema from the lane's `kind:"data"` result
	/// payload (§19.2): array of {name, display_name, type, levels?, all_integer?}.
	/// Emits schemaChanged().
	void				applySchema(const std::string & datasetId, uint64_t rows, const Json::Value & schema, const std::string & sourcePath);

	// metadata queries (the former DataSetPackage metadata-group surface)
	size_t				columnCount() const	{ return _columns.size();	}
	stringvec			columnNames() const;
	std::map<std::string, columnType> columnTypesMap() const;
	int					columnIndex(const std::string & name) const;	///< -1 when absent
	bool				isColumnNameFree(const std::string & name) const;
	const ColumnInfo *	column(const std::string & name) const;
	const ColumnInfo *	columnAt(size_t index) const;

signals:
	/// The one "data changed" bus. Phase A: emitted on applySchema; later also on
	/// data_changed/data_update from the orchestrator and on bookkeeping mutations.
	void				schemaChanged();

private:
	std::string				_datasetId;
	uint64_t				_rows = 0;
	std::string				_sourcePath;
	std::vector<ColumnInfo>	_columns;
	/// name -> position in _columns, rebuilt in applySchema (wide-data fix, 2026-08-15).
	/// Every by-name VariableInfo query (getColumnIndex/provideInfo) is O(1) through this;
	/// without it the frontend's all-columns enumerations degrade to O(k²) at 10k columns.
	std::unordered_map<std::string, size_t>	_columnIndex;
};

#endif // DATAMODEL_H
