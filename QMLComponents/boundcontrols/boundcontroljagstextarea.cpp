//
// Copyright (C) 2013-2020 University of Amsterdam
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

#include "boundcontroljagstextarea.h"
#include "controls/textareabase.h"
#include "variableinfo.h"
#include "stringutils.h"
#include <algorithm>
#include <cctype>
#include <vector>

namespace
{
	// R/JAGS identifier chars for token-boundary purposes (legacy encodeRScript: [\.A-Za-z0-9_]).
	bool isRNameChar(char c)
	{
		return isalnum(static_cast<unsigned char>(c)) || c == '.' || c == '_';
	}

	// Free-token column-reference extraction — detection semantics of legacy
	// ColumnEncoder::encodeRScript (columnencoder.cpp:440-507), NO rewriting: raw UTF-8 names
	// cross the wire, the runner aliases (HANDOVER-runner-data-pruning.md §2.1/§3.7). Same
	// helper as boundcontrolrlangtextarea.cpp (kept local to both to avoid a new shared
	// header for this pass).
	stringset findFreeColumnRefs(const std::string & text, stringvec names, std::vector<bool> & consumed)
	{
		stringset found;

		std::vector<bool>	inString(text.size(), false);
		bool				inside = false;
		char				delim = 0;
		for (size_t i = 0; i < text.size(); i++)
		{
			char c = text[i];
			if (!inside && (c == '"' || c == '\''))	{ inside = true; delim = c; inString[i] = true; }
			else if (inside)							{ inString[i] = true; if (c == delim) inside = false; }
		}

		std::sort(names.begin(), names.end(),
			[](const std::string & l, const std::string & r) { return l.size() > r.size(); });

		for (const std::string & name : names)
		{
			if (name.empty()) continue;
			size_t pos = 0;
			while ((pos = text.find(name, pos)) != std::string::npos)
			{
				size_t	end		= pos + name.size();
				bool	freePos	= (pos == 0 || !isRNameChar(text[pos - 1])) &&
								  (end >= text.size() || !isRNameChar(text[end])) &&
								  !inString[pos] && !consumed[pos];
				if (freePos)
				{
					found.insert(name);
					for (size_t k = pos; k < end; k++) consumed[k] = true;
					pos = end;
				}
				else
					pos++;
			}
		}
		return found;
	}
}

void BoundControlJAGSTextArea::bindTo(const Json::Value &value)
{
	if (value.type() != Json::objectValue)	return;
	BoundControlBase::bindTo(value);

	_textArea->setText(tq(value["modelOriginal"].asString()));

	checkSyntax();

}

Json::Value BoundControlJAGSTextArea::createJson() const
{
	Json::Value result;
	std::string text = _textArea->text().toStdString();

	result["modelOriginal"] = text;
	result["model"]			= text;
	result["columns"]		= Json::Value(Json::arrayValue);
	result["parameters"]	= Json::Value(Json::arrayValue);

	return result;
}

bool BoundControlJAGSTextArea::isJsonValid(const Json::Value &value) const
{
	if (!value.isObject())					return false;
	if (!value["modelOriginal"].isString())	return false;
	if (!value["model"].isString())			return false;
	//if (!value["columns"].isArray())		return false;
	//if (!value["parameters"].isArray())		return false;

	return true;
}

void BoundControlJAGSTextArea::checkSyntax()
{
	QString text = _textArea->text();

	// google: jags_user_manual (4.3.0) for documentation on JAGS symbols

	// NEO (§3.7): extraction against the DataModel names, no encoding. Raw UTF-8 names cross
	// the wire; the runner aliases them statelessly (§2.2) and rewrites the model text (§3.2).
	_usedColumnNames.clear();
	std::string stripped = stringUtils::stripRComments(fq(text));

	VariableInfoProvider * provider = VariableInfo::info() ? VariableInfo::info()->provider() : nullptr;
	stringvec columnNames;
	if (provider)
		for (const QString & name : provider->provideInfo(VariableInfo::VariableNames).toStringList())
			columnNames.push_back(fq(name));

	std::vector<bool> consumed(stripped.size(), false);
	_usedColumnNames = findFreeColumnRefs(stripped, columnNames, consumed);

	QRegularExpression relationSymbol = QRegularExpression("<-|=|~");
	QStringList textByLine = tq(stripped).split(QRegularExpression(";|\n"));
	_usedParameters.clear();

	for (QString & line : textByLine)
	{
		// comments were already removed by stringUtils::stripRComments
		if (line.contains(relationSymbol))
		{
			// extract parameter and remove whitespace
			QString paramName = line.split(relationSymbol).first().trimmed();
			// remove any link functions (cloglog|log|probit|logit)
			if (paramName.contains("(") && paramName.contains(")"))
			{
				int idxStart, idxEnd;
				idxStart = paramName.indexOf("(") + 1;
				idxEnd   = paramName.indexOf(")") - idxStart;
                paramName = paramName.mid(idxStart, idxEnd);
			}

			// get rid of any indexing
			if (paramName.contains("["))
                paramName = paramName.left(paramName.indexOf("["));

			// NEO: a parameter is an LHS name that is NOT a dataset column (legacy excluded
			// encoded column tokens via shouldDecode; with raw names the exclusion is simply
			// "is it a referenced column").
			if (paramName != "" && _usedColumnNames.count(fq(paramName)) == 0)
				_usedParameters.insert(paramName);

		}
	}

	Json::Value boundValue(Json::objectValue);

	boundValue["modelOriginal"] = text.toStdString();
	boundValue["model"] = stripped;		// raw text, comments stripped; the runner rewrites (§3.2)
	Json::Value columns(Json::arrayValue);
	for (const std::string& column : _usedColumnNames)
		columns.append(column);		// raw names; the runner aliases (§3.7)
	boundValue["columns"] = columns;
	Json::Value parameters(Json::arrayValue);
	for (const QString& parameter : _usedParameters)
		parameters.append(parameter.toStdString());
	boundValue["parameters"] = parameters;

	setBoundValue(boundValue);

	ListModelTermsAvailable* model = _textArea->availableModel();

	// Do not init the model terms when not necessary: this can call the resetBoundValues that calls checkSyntax
	if (model->terms() != _usedParameters.values())
		model->initTerms(_usedParameters.values());
}


