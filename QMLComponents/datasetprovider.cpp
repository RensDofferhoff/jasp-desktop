//
// Copyright (C) 2013-2025 University of Amsterdam
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
// <https://www.gnu.org/licenses/>.
//

#include "datasetprovider.h"
#include "columnencoder.h"
#include "columnutils.h"
#include "qutils.h"

#include <algorithm>
#include <memory>
#include <set>

DataSetProvider		*	DataSetProvider::_singleton		= nullptr;

DataSetProvider* DataSetProvider::getProvider(bool inMemory, bool reset, QObject* parent)
{
	if (!_singleton)
		_singleton = new DataSetProvider(inMemory, parent);
	else if (_singleton->_inMemory != inMemory)
	{
		delete _singleton;
		_singleton = new DataSetProvider(inMemory, parent);
	}
	else if (reset)
		_singleton->resetDataSet();

	return _singleton;
}

DataSetProvider::~DataSetProvider()
{
	assert(_singleton == this);
	delete _workspace;
	_singleton = nullptr;
}

DataSetProvider::DataSetProvider(bool inMemory, QObject *parent) : QAbstractTableModel(parent), _inMemory(inMemory)
{
	// The excision, Cut 3: the provider no longer owns a DatabaseInterface — its sqlite-backed
	// loadDatabase/closeDatabase died with the class (.jasp persistence returns in a later NEO era).
	_workspace = new Workspace();

	// The excision, Cut 6: in the engine/test worlds there is no MainWindow/ColumnsModel —
	// this provider (schema-correct since Cut 5) serves the forms itself.
	_workspace->setFormProvider(this);

	new VariableInfo(this);
	_singleton = this;
}

void DataSetProvider::resetDataSet()
{
	if (_workspace)
		delete _workspace;
	
	_workspace = new Workspace(this);
	_workspace->setFormProvider(this);	// the excision, Cut 6: re-register on the fresh Workspace
	_workspace->createDataSet();
}

int	DataSetProvider::rowCount(const QModelIndex &) const
{
	return dataSet()->columnCount();
}

int	DataSetProvider::columnCount(const QModelIndex &) const
{
	return 1;
}

QVariant DataSetProvider::data(const QModelIndex & index, int role) const
{
	const ColumnInfo * info = index.row() >= rowCount() ? nullptr : dataSet()->schemaColumnAt(size_t(index.row()));

	if (!info)							return QVariant();
	else if (role == Qt::DisplayRole)	return tq(info->name);
	else								return QVariant();
}

// The excision, Cut 5: loadDataSet built legacy Columns via initFromLookups — Column is
// gone. The provider now plays a LANE dataset instead (applySchema), exactly what the
// orchestrator would deliver on an open: the string data become schema metadata (types,
// level sets, distinct counts), which is all the forms/QML layer may ask for. There are no
// row values to serve — the schema is the truth, the grid reads the view lane.

void DataSetProvider::loadDataSet(const std::map<std::string, stringvec > & dataSetStrings, int threshold, bool orderLabelsByValue)
{
	Q_UNUSED(threshold);
	Q_UNUSED(orderLabelsByValue);

	if (!dataSet())
		_workspace->createDataSet();

	DataSet * dataSet = this->dataSet();

	size_t		rows	= 0;
	Json::Value	schema(Json::arrayValue);

	for (const auto & [name, values] : dataSetStrings)
	{
		rows = std::max(rows, values.size());

		// Infer the wire type the way the lane would: every value numeric → scale, else a
		// categorical whose distinct values are its levels.
		bool		allNumeric = !values.empty();
		stringvec	levels;

		for (const std::string & val : values)
		{
			double dummy;
			if (!ColumnUtils::getDoubleValue(val, dummy))
			{
				allNumeric = false;
				if (std::find(levels.begin(), levels.end(), val) == levels.end())
					levels.push_back(val);
			}
		}

		Json::Value col(Json::objectValue);
		col["name"]			= name;
		col["display_name"]	= name;
		col["value_count"]	= Json::UInt64(values.size());

		if (allNumeric)
		{
			col["type"]			= "scale";
			col["distinct_count"]	= Json::UInt64(std::set<std::string>(values.begin(), values.end()).size());
		}
		else
		{
			col["type"]	= "nominal";
			Json::Value jsonLevels(Json::arrayValue);
			for (const std::string & level : levels)
				jsonLevels.append(level);
			col["levels"]			= jsonLevels;
			col["distinct_count"]	= Json::UInt64(levels.size());
		}

		schema.append(col);
	}

	dataSet->applySchema("datasetprovider-fixture", rows, schema, "");

	// The excision, Cut 6: this provider serves the forms — announce the fresh schema through
	// the provider contract so any already-created VariableInfo re-queries (mirrors what
	// ColumnsModel::bindLane does on schemaChanged in the desktop world).
	emit infoSignaller()->refresh();
	emit infoSignaller()->dataSetChanged();
	emit infoSignaller()->rowCountChanged();
	emit infoSignaller()->variableCountChanged();
}

QStringList DataSetProvider::_getColumnNames() const
{
	return tq(dataSet()->getColumnNames());
}


QVariant DataSetProvider::provideInfo(varInfoType info, const QString& colName, int row) const
{
	Q_UNUSED(row);

	try
	{
		const ColumnInfo * column = dataSet()->schemaColumn(fq(colName));

		switch(info)
		{
		case varInfoType::VariableType:				return	int(!column ? columnType::unknown : column->type);
		case varInfoType::DoubleValues:				return	QVariantList();			// no row values on the schema path
		case varInfoType::TotalNumericValues:		return	!column ? 0 : (column->type == columnType::scale ? int(column->distinctCount) : column->numericLevels);
		case varInfoType::TotalLevels:					return	!column ? 0 : (column->type == columnType::scale ? int(column->distinctCount) : int(column->levels.size()));
		case varInfoType::Labels:						return	!column ? QStringList() : tq(column->levels);
		case varInfoType::NameRole:					return	Qt::DisplayRole;
		case varInfoType::DataSetRowCount:				return  int(dataSet()->rowCount());
		case varInfoType::DataSetValue:				return	"";						// no row values on the schema path
		case varInfoType::DataSetValues:				return	QStringList();
		case varInfoType::MaxWidth:					return	100;
		case varInfoType::SignalsBlocked:				return	false;
		case varInfoType::VariableNames:				return	_getColumnNames();
		case varInfoType::DataAvailable:				return	dataSet()->columnCount() > 0;
		case varInfoType::PreviewScale:				return	"";
		case varInfoType::PreviewOrdinal:				return	"";
		case varInfoType::PreviewNominal:				return	"";
		case varInfoType::ColumnDescription:		return	!column ? QString() : tq(column->description);
		case varInfoType::DisplayName:			return	!column ? QString() : tq(column->displayName);	// D11 decode: token -> human name
		case varInfoType::DataSetPointer:			return	QVariant::fromValue<void*>(dataSet());


		default: break;
		}
	}
	catch(std::exception & e)
	{
		throw e;
	}
	return QVariant("");
}

bool DataSetProvider::absorbInfo(varInfoType info, const QString &colName, int row, QVariant value)
{
	// The excision, Cut 5: writes went into legacy Column storage — with Column gone this
	// provider is read-only schema service (the grid edits through DataEditCommand).
	Q_UNUSED(colName);
	Q_UNUSED(row);
	Q_UNUSED(value);
	Q_UNUSED(info);
	return false;
}
