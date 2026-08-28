/// ColumnInfo — the wire-schema description of one dataset column, as delivered by the
/// orchestrator's kind:"data" payloads (data-model-design.md §2). Owned by DataSet after
/// the multi-dataset fold (moved from Desktop/data/datamodel.h — that class is gone; the
/// schema is DataSet's now). The lane guarantees every count here; the frontend never
/// counts anything itself.
#ifndef COLUMNINFO_H
#define COLUMNINFO_H

#include <string>
#include <vector>

#include "columntype.h"
#include "utils.h"

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

#endif // COLUMNINFO_H
