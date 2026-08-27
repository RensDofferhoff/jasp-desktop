#include "datamodel.h"

DataModel::DataModel(QObject * parent)
	: QObject(parent)
{
}

void DataModel::applySchema(const std::string & datasetId, uint64_t rows, const Json::Value & schema, const std::string & sourcePath)
{
	_datasetId		= datasetId;
	_rows			= rows;
	_sourcePath		= sourcePath;
	_columns.clear();
	_columnIndex.clear();

	if (schema.isArray())
		for (const Json::Value & col : schema)
		{
			ColumnInfo info;

			info.name			= col.get("name", "").asString();
			info.displayName	= col.get("display_name", info.name).asString();

			const std::string wireType = col.get("type", "scale").asString();
			info.type =	wireType == "scale"		? columnType::scale
					:	wireType == "ordinal"	? columnType::ordinal
					:							  columnType::nominal;

			info.allInteger		= col.get("all_integer", false).asBool();

			// Constraint-check stats (data-model-design.md §2): non-empty count + distinct count
			// from the lane — they answer every form levels/numeric threshold; nothing in the
			// frontend ever counts distinct values itself.
			if (col.isMember("value_count"))
				info.valueCount = col["value_count"].asUInt64();
			if (col.isMember("distinct_count"))
				info.distinctCount = col["distinct_count"].asUInt64();
			if (col.isMember("numeric_levels"))
				info.numericLevels = col["numeric_levels"].asInt();

			if (col.isMember("levels") && col["levels"].isArray())
				for (const Json::Value & level : col["levels"])
					info.levels.push_back(level.asString());

			_columnIndex.emplace(info.name, _columns.size());	// first-wins, same semantics as the old linear scan
			_columns.push_back(std::move(info));
		}

	emit schemaChanged();
}

stringvec DataModel::columnNames() const
{
	stringvec names;
	names.reserve(_columns.size());
	for (const ColumnInfo & col : _columns)
		names.push_back(col.name);
	return names;
}

std::map<std::string, columnType> DataModel::columnTypesMap() const
{
	std::map<std::string, columnType> types;
	for (const ColumnInfo & col : _columns)
		types[col.name] = col.type;
	return types;
}

int DataModel::columnIndex(const std::string & name) const
{
	const auto found = _columnIndex.find(name);
	return found == _columnIndex.end() ? -1 : int(found->second);
}

bool DataModel::isColumnNameFree(const std::string & name) const
{
	return columnIndex(name) == -1;
}

const ColumnInfo * DataModel::column(const std::string & name) const
{
	const int idx = columnIndex(name);
	return idx < 0 ? nullptr : &_columns[size_t(idx)];
}

const ColumnInfo * DataModel::columnAt(size_t index) const
{
	return index < _columns.size() ? &_columns[index] : nullptr;
}
