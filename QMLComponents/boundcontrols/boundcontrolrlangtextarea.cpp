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

#include "boundcontrolrlangtextarea.h"
#include "controls/textareabase.h"
#include "log.h"
#include "variableinfo.h"
#include "stringutils.h"
#include "analysisform.h"
#include <QQuickTextDocument>
#include <algorithm>
#include <cctype>
#include <vector>

namespace
{
	// R identifier chars for token-boundary purposes (legacy encodeRScript: [\.A-Za-z0-9_]).
	bool isRNameChar(char c)
	{
		return isalnum(static_cast<unsigned char>(c)) || c == '.' || c == '_';
	}

	// Free-token extraction with the DETECTION semantics of legacy
	// ColumnEncoder::encodeRScript (columnencoder.cpp:440-507): a name matches only with a
	// non-name char (or text edge) on both sides, occurrences inside string literals are
	// skipped (escapes not considered, same as legacy), and longer names consume their
	// ranges first so partial names cannot shadow them. Extraction only — NEO does NOT
	// rewrite text in the frontend; aliasing happens in the runner
	// (HANDOVER-runner-data-pruning.md §2.1/§3.2/§3.7). `consumed` marks ranges already
	// claimed by an earlier (longer / prefixed) pass.
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

BoundControlRlangTextArea::BoundControlRlangTextArea(TextAreaBase *textArea, RLangType type)
	: BoundControlTextArea(textArea), _langType(type)
{
	QVariant textDocumentVariant = textArea->property("textDocument");
	QQuickTextDocument* textDocumentQQuick = textDocumentVariant.value<QQuickTextDocument *>();
	if (textDocumentQQuick)
	{
		QTextDocument* doc = textDocumentQQuick->textDocument();
        _rLangHighlighter = new RSyntaxHighlighter(doc);
		//connect(doc, &QTextDocument::contentsChanged, this, &BoundQMLTextArea::contentsChangedHandler);
	}
	else
		Log::log()  << "No document object found!" << std::endl;
}

void BoundControlRlangTextArea::bindTo(const Json::Value &value)
{
	if (value.type() != Json::objectValue)	return;
	BoundControlBase::bindTo(value);

	_textArea->setText(tq(value["modelOriginal"].asString()));

	checkSyntax();

}

void BoundControlRlangTextArea::resetBoundValue()
{
	_setBoundValues(false);
}

Json::Value BoundControlRlangTextArea::createJson() const
{
	Json::Value result;
	std::string text = _textArea->text().toStdString();

	result["modelOriginal"] = text;
	result["model"]			= text;
	result["columns"]		= Json::Value(Json::arrayValue);

	return result;
}

bool BoundControlRlangTextArea::isJsonValid(const Json::Value &value) const
{
	if (!value.isObject())					return false;
	if (!value["modelOriginal"].isString())	return false;

	return true;
}

void BoundControlRlangTextArea::checkSyntax()
{
	QString text = _textArea->text();

	// NEO (§3.7): EXTRACTION only, against the DataModel names — no encoding, no rewriting.
	// Raw UTF-8 names cross the wire; the runner aliases them statelessly (§2.2) and rewrites
	// the model text (§3.2 step 6).
	_extractUsedColumnNames(stringUtils::stripRComments(fq(text)));

	// Live syntax validation (legacy ran jaspSem:::checkLavaanModel / checkCSemModel /
	// jaspMetaAnalysis::checkMetaModel here via runRScript, :207-216 of the old code) needs
	// the reserved `module_call` kind (§6 — spec'd, not built in this pass; §7 model-
	// exception debt). Until then validation happens at analysis run time: lavaan errors
	// surface in the results. UX regression, not correctness.
	_setBoundValues();
}

void BoundControlRlangTextArea::_extractUsedColumnNames(const std::string & text)
{
	_prefixedUsedColumnNames.clear();
	_noPrefixUsedColumnNames.clear();

	VariableInfoProvider * provider = VariableInfo::info() ? VariableInfo::info()->provider() : nullptr;
	if (!provider)
	{
		Log::log() << "BoundControlRlangTextArea: no variable-info provider, skipping column extraction" << std::endl;
		return;
	}

	stringvec columnNames;
	for (const QString & name : provider->provideInfo(VariableInfo::VariableNames).toStringList())
		columnNames.push_back(fq(name));
	if (columnNames.empty()) return;

	std::vector<bool> consumed(text.size(), false);

	// Prefixed references first (legacy allowedVarPrefixes, e.g. "data." for JAGS-flavoured
	// syntax): "prefix + name" with a free boundary on both sides; longest prefixes first so
	// shorter ones cannot shadow them.
	stringvec prefixes(_allowedVarPrefixes.begin(), _allowedVarPrefixes.end());
	std::sort(prefixes.begin(), prefixes.end(),
		[](const std::string & l, const std::string & r) { return l.size() > r.size(); });
	for (const std::string & prefix : prefixes)
	{
		if (prefix.empty()) continue;
		stringvec prefixedNames;
		for (const std::string & name : columnNames) prefixedNames.push_back(prefix + name);
		stringset hits = findFreeColumnRefs(text, prefixedNames, consumed);
		if (hits.empty()) continue;
		stringset cols;
		for (const std::string & hit : hits) cols.insert(hit.substr(prefix.size()));
		_prefixedUsedColumnNames[prefix] = cols;
	}

	// Unprefixed references on the remaining text.
	_noPrefixUsedColumnNames = findFreeColumnRefs(text, columnNames, consumed);
}

QString BoundControlRlangTextArea::rScriptDoneHandler(const QString & result)
{
	if (!result.isEmpty())
		return result;

	_setBoundValues();
	return QString();
}

void BoundControlRlangTextArea::_setBoundValues(bool setModel)
{
	Json::Value boundValue(Json::objectValue);

	std::string text = _textArea->text().toStdString();

	// NEO (§3.7): the wire carries RAW text in both twins. `model` stays — modules consume
	// options$model (legacy: the frontend pre-encoded it; NEO: the runner rewrites it in
	// place with aliases, §3.2 step 6). `columns`/`prefixedColumns` carry raw names too;
	// the runner aliases them per the parallel types (§3.7 dispositions).
	boundValue["modelOriginal"] = text;
	boundValue["model"]			= text;

	Json::Value columns(Json::arrayValue),
				value(Json::arrayValue);
	Terms		terms;

	for (const std::string& column : _noPrefixUsedColumnNames)
	{
		terms.add(Term(column, _textArea->getVariableType(tq(column))));
		columns.append(column);
		value.append(column);
	}

	if (setModel && _textArea->model())
		_textArea->model()->initTerms(terms);
	boundValue["columns"]	= columns;
	boundValue["value"]		= value;
	boundValue["types"]		= terms.types();
	boundValue["optionKey"] = "value";

	Json::Value prefixedColumns(Json::objectValue);
	for(auto& prefixSet : _prefixedUsedColumnNames) {
		prefixedColumns[prefixSet.first] = Json::Value(Json::arrayValue);
		for (const std::string& column : prefixSet.second) {
			prefixedColumns[prefixSet.first].append(column);
		}
	}
	boundValue["prefixedColumns"] = prefixedColumns;

	setBoundValue(boundValue, !_control->form()->wasUpgraded());
}

const char* BoundControlRlangTextArea::_checkSyntaxRFunctionName()
{
	switch (_langType)
	{
	case RLangType::CSem:		return "jaspSem:::checkCSemModel";
	case RLangType::MetaSem:	return "jaspMetaAnalysis::checkMetaModel";
	case RLangType::Lavaan:		return "jaspSem:::checkLavaanModel";
	default:					return "";
	}
}
