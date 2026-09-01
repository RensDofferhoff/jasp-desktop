#include "dataedit.h"

#include <QLocale>

#include "jaspclient.h"
#include "log.h"

// ── Op builders (§3) ────────────────────────────────────────────────────────────────────

Json::Value DataEdit::insertBlockOp(uint64_t row, uint64_t col)
{
	// Absent target_schema = the lane recomputes (D4): absorption/promotion, D5 inference
	// for new columns — exactly the undeclared paste the v1 UI starts from. Declarations
	// arrive with the paste-dialog UX, not before it is needed.
	Json::Value op(Json::objectValue);
	op["op"]	= "insert_block";
	op["row"]	= Json::UInt64(row);
	op["col"]	= Json::UInt64(col);
	return op;
}

Json::Value DataEdit::applyInverseOp(const Json::Value & inverseMeta)
{
	Json::Value op(Json::objectValue);
	op["op"]		= "apply_inverse";
	op["inverse"]	= inverseMeta;		// the stored meta, verbatim (D10)
	return op;
}

QString DataEdit::wireTypeOf(columnType type)
{
	switch (type)
	{
	case columnType::scale:		return QStringLiteral("scale");
	case columnType::ordinal:	return QStringLiteral("ordinal");
	case columnType::nominal:
	case columnType::nominalText:	return QStringLiteral("nominal");
	default:						return QString();	// unknown — not representable on the wire
	}
}

Json::Value DataEdit::schemaChangeTypeOp(DataSet * dataSet, const std::set<std::string> & columnNames, columnType newType)
{
	// d6's shape: one entry per column of the POST-EDIT schema, in order, each carrying
	// its CURRENT field name as the identity (`name`); absent fields mean keep. Only the
	// targeted entries carry `type` — a retype, nothing else (rename/reorder/relabel are
	// other gestures' entries).
	const QString wireType = wireTypeOf(newType);
	Json::Value targetSchema(Json::arrayValue);
	for (const ColumnInfo & info : dataSet->schema())
	{
		Json::Value entry(Json::objectValue);
		entry["name"] = info.name;
		if (columnNames.count(info.name))
			entry["type"] = wireType.toStdString();
		targetSchema.append(entry);
	}

	Json::Value op(Json::objectValue);
	op["op"]				= "schema_change";
	op["target_schema"]	= targetSchema;
	return op;
}

Json::Value DataEdit::schemaChangeRenameOp(DataSet * dataSet, const std::string & currentName, const std::string & newDisplayName)
{
	// Same count-match shape as the retype builder: every column by its CURRENT field
	// name, `display_name` declared only on the target. Per P4 the lane derives the new
	// field name from the declared display name (uniquified per P6) — Name and Long name
	// move together; absent any consumer of an independent field name, that is the v1
	// rename gesture.
	Json::Value targetSchema(Json::arrayValue);
	for (const ColumnInfo & info : dataSet->schema())
	{
		Json::Value entry(Json::objectValue);
		entry["name"] = info.name;
		if (info.name == currentName)
			entry["display_name"] = newDisplayName;
		targetSchema.append(entry);
	}

	Json::Value op(Json::objectValue);
	op["op"]				= "schema_change";
	op["target_schema"]	= targetSchema;
	return op;
}

Json::Value DataEdit::insertColsOp(uint64_t at, const std::string & name, columnType type)
{
	// One NewColumnSpec (d5): name + declared type when representable. P7: a non-empty
	// spec list; names are INPUTS here (uniquified by the lane per P6 — unlike a paste
	// window's declared target_schema, where a name is an assertion).
	Json::Value spec(Json::objectValue);
	spec["name"]	= name;
	const QString wireType = wireTypeOf(type);
	if (!wireType.isEmpty())
		spec["type"]	= wireType.toStdString();

	Json::Value columns(Json::arrayValue);
	columns.append(spec);

	Json::Value op(Json::objectValue);
	op["op"]		= "insert_cols";
	op["at"]			= Json::UInt64(at);
	op["columns"]	= columns;
	return op;
}

// ── §1.2 authoring ──────────────────────────────────────────────────────────────────────

QString DataEdit::escapeCell(const QVariant & cell)
{
	// A null cell ships as the whole-cell marker `\N` (a literal "\N" cell would escape
	// its backslash — `\\N` — unambiguous by construction, format doc §1.2). Doubles ride
	// as their shortest round-trip text so the lane's locale parse sees what the user saw.
	if (cell.isNull())
		return QStringLiteral("\\N");

	QString text;
	if (cell.userType() == QMetaType::Double)
	{
		// The lane parses under the ingest locale we submit with (system decimal); render
		// with the same locale so what the user typed is what round-trips.
		text = QLocale::system().toString(cell.toDouble(), 'g', 10);
	}
	else
		text = cell.toString();

	QString out;
	out.reserve(text.size());
	for (const QChar c : text)
	{
		switch (c.unicode())
		{
		case u'\\':	out += QStringLiteral("\\\\");	break;
		case u'\t':	out += QStringLiteral("\\t");	break;
		case u'\n':	out += QStringLiteral("\\n");	break;
		case u'\r':	out += QStringLiteral("\\r");	break;
		default:	out += c;
		}
	}
	return out;
}

QByteArray DataEdit::tsvFromCells(const std::vector<std::vector<QString>> & values)
{
	// values[col][row] (the paste rectangle as the proxy builds it) → §1.2: one line per
	// row, cells tab-separated, EVERY row LF-terminated (the lane's shape check requires
	// the trailing LF; an empty row is a single empty cell, which the null spellings null).
	if (values.empty())
		return QByteArray();

	const size_t cols = values.size(),
				 rows  = values[0].size();
	QString block;
	for (size_t r = 0; r < rows; ++r)
	{
		for (size_t c = 0; c < cols; ++c)
		{
			if (c)	block += u'\t';
			block += values[c][r];
		}
		block += u'\n';
	}
	return block.toUtf8();
}

// ── The undo command ────────────────────────────────────────────────────────────────────

DataEditCommand::DataEditCommand(DataSet * dataSet, const Json::Value & editOp, QByteArray tail, const QString & text)
	: UndoModelCommand(dataSet)
	, _editOp(editOp)
	, _tail(std::move(tail))
	, _text(text)
{
	setText(_text);		// the undo/redo menu label — frontend-authored, from the forward gesture (§5)
}

void DataEditCommand::submit(bool isUndo)
{
	DataSet * ds = dataSet();
	if (!ds || !ds->isOpen())
	{
		Log::log() << "DataEditCommand: dataset gone or not open — " << _text.toStdString() << " dropped" << std::endl;
		return;
	}

	// UNDO submits the stored blob VERBATIM (D10) — meta in the op, IPC bytes in the
	// tail. If no inverse was ever captured (the forward edit failed or never completed),
	// there is nothing to reverse: refuse loudly rather than guess.
	if (isUndo && _inverseMeta.isNull())
	{
		Log::log() << "DataEditCommand: no inverse stored — cannot undo '" << _text.toStdString() << "'" << std::endl;
		return;
	}

	const Json::Value	op		= isUndo ? DataEdit::applyInverseOp(_inverseMeta) : _editOp;
	const QByteArray	tail	= isUndo ? _inverseBytes : _tail;

	Log::log() << "DataEditCommand: submitting " << (isUndo ? "undo" : "edit") << " '" << _text.toStdString()
				   << "' for " << ds->datasetId() << " at revision " << ds->laneRevision() << std::endl;

	JaspClient::client()->submitDataEdit(
				QString::fromStdString(ds->datasetId()),
				ds->laneRevision(),			// the D11 echo, read at submit time (it only climbs)
				op, tail,
				[this, isUndo](const JaspClient::Result & result)
	{
		if (result.status == "complete")
		{
			// File the forward edit's inverse — the undo blob. (The UNDO's own result
			// also carries an inverse — the redo material — which redo does not use: it
			// re-submits the op. Keeping the original blob is the correct choice under
			// LIFO; see the class comment.)
			if (!isUndo)
			{
				_inverseMeta	= result.inverseMeta;
				_inverseBytes	= result.binary;
				Log::log() << "DataEditCommand: '" << _text.toStdString() << "' applied — revision "
					   << result.datasetRevision << ", inverse " << result.binary.size() << " bytes stored" << std::endl;
			}
			return;
		}

		// The orchestrator's pending-ack (§25.5 — the edit chain queues one edit per dataset
		// behind the in-flight one; also the parked-work marker): NOT a failure — keep
		// waiting for the terminal result.
		if (result.status == "running")
			return;

		// v1 failure surfacing: log loudly with the structured detail when present.
		// A refused edit changed nothing (§3 atomicity); a stale_edit means the revision
		// raced (two editors / a lost push) — the data_changed rail re-syncs the view.
		Log::log() << "DataEditCommand: " << (isUndo ? "undo" : "edit") << " '" << _text.toStdString()
				   << "' failed (" << result.status << "): " << result.message << std::endl;
	});
}

void DataEditCommand::redo()
{
	submit(false);
}

void DataEditCommand::undo()
{
	submit(true);
}
