#ifndef DATAEDIT_H
#define DATAEDIT_H

#include <QByteArray>
#include <QString>
#include <QVariant>

#include <json/json.h>

#include "undostack.h"
#include "dataset.h"

/// The data-edit client vocabulary (data-edit-design §2/§3/§5): everything wire-shaped
/// about editing lives HERE, beside JaspClient — the op JSON shapes, the §1.2 TSV
/// authoring, the inverse's storage, and the one undo command that submits them. The
/// rest of the frontend (proxy, models, view) asks in UI terms and never sees the wire.
namespace DataEdit
{
	// ── Op builders (§3; adjacently-tagged objects as the wire expects them) ──────────

	/// `insert_block` at anchor `(row, col)`; the §1.2 TSV cells ride the frame tail.
	Json::Value	insertBlockOp(uint64_t row, uint64_t col);

	/// `apply_inverse` — submits a previously returned inverse blob VERBATIM (D10): the
	/// meta rides the `inverse` field, the IPC bytes ride the frame tail.
	Json::Value	applyInverseOp(const Json::Value & inverseMeta);

	// ── §1.2 authoring (the mirror of DataViewBuffer::unescapeCell) ──────────────────

	/// Escape one authored cell: `\` → `\\`, TAB → `\t`, LF → `\n`, CR → `\r`. A null cell
	/// (QVariant null / an invalid double) ships as the whole-cell marker `\N`; an EMPTY
	/// string stays empty — the lane's null spellings null it again (I3: the same
	/// spellings that nulled at open null again).
	QString		escapeCell(const QVariant & cell);

	/// The §1.2 block for a paste rectangle `values[col][row]`: cells tab-separated,
	/// every row LF-terminated (the shape rule the lane's parser checks).
	QByteArray	tsvFromCells(const std::vector<std::vector<QString>> & values);
}

/// The edit-era undo command (§5/§7): ONE command wraps ONE wire edit — the op + its
/// binary tail, authored once at the commit boundary (a paste is ONE insert_block, not N
/// cell commands — coalescing lives at the capture side by design).
///
///  - `redo()` SUBMITS the op through JaspClient (`revision = DataSet::laneRevision()`
///    read AT CALL TIME — the D11 echo; the stack's push calls redo() immediately, and a
///    replayed redo after undo re-reads it because the revision only climbs). The
///    command NEVER writes a cell itself.
///  - the result's inverse — `{format, base_revision, ops}` meta + the IPC bytes riding
///    the frame tail — is stored VERBATIM (D10: opaque, session-bound, never
///    interpreted, never merged).
///  - `undo()` resubmits that stored blob via `apply_inverse`, verbatim.
///
/// Asymmetry note (the design's pin): redo RE-SUBMITS THE OP rather than the undo
/// result's own blob — sound under the stack's strict LIFO (state ≡ pre-edit at redo
/// time and the op is deterministic), and it keeps the stored blob the ONE thing undo
/// ever needs: the blob captured against the exact state it reverses. An
/// undo→redo→undo cycle therefore reuses the ORIGINAL blob every time — the correct
/// one, for the same LIFO reason.
class DataEditCommand : public UndoModelCommand
{
public:
	/// `editOp` = a DataEdit::* op builder's product; `tail` = its binary part (§1.2 TSV
	/// cells for insert_block; empty for metadata ops); `text` = the undo-menu label
	/// (frontend-authored, from the forward gesture).
	DataEditCommand(DataSet * dataSet, const Json::Value & editOp, QByteArray tail, const QString & text);

	void	undo()					override;
	void	redo()					override;

private:
	/// Submit the forward op (or the stored inverse, for undo) and file the result's
	/// inverse on completion. Shared by redo()/undo() so failure logging lives once.
	void			submit(bool isUndo);

	Json::Value		_editOp;			///< the forward op, verbatim
	QByteArray		_tail;				///< its binary part (§1.2 TSV / empty)
	Json::Value		_inverseMeta;		///< the result's inverse meta; null until the forward edit completes
	QByteArray		_inverseBytes;		///< …and its IPC bytes — the undo blob, stored verbatim
	QString			_text;
};

#endif // DATAEDIT_H
