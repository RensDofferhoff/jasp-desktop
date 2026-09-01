#ifndef UNDOSTACK_H
#define UNDOSTACK_H

/*
	Author: JASP development team

	The excision, Cut 4: the legacy Column-serializing undo-command family is gone —
	every undoable gesture on NEO lane data is a `DataEditCommand` (Desktop/jaspclient/
	dataedit.h). What remains here is the `UndoModelCommand` base (dataset identity +
	the undo-material footprint the byte cap sums) and the `UndoStack` itself: macro
	start/end, the ~250 MB drop-oldest byte cap (data-edit-design §11.4), and the
	singleton plumbing. Filters/computed columns return later as derivations
	(`ChangeKind::derived`) with their own commands on this same rail.
*/

#include <QUndoStack>
#include <QUndoCommand>
#include <set>
#include <map>
#include <string>
#include "column.h"

class DataSet;
class Workspace;
class Column;

class UndoModelCommand : public QUndoCommand
{
public:
						UndoModelCommand(DataSet * dataSet = nullptr);

	/// The command's resident UNDO-material footprint in bytes (the inverse blob + any
	/// stored forward tail). The stack sums this to enforce the byte cap (the ~250 MB
	/// drop-oldest policy, data-edit-design §11.4).
	virtual size_t		undoBytes() const					{ return 0; }

	bool				dataSetStillExists()				const;
	DataSet		*		dataSet()							const;

protected:
	int					_dataSetID	= -1;
};


class UndoStack : public QUndoStack
{
	Q_OBJECT
public:
	UndoStack(QObject* parent = nullptr);

	static UndoStack*	singleton() { return _currentUndoStack; }
	static void			setCurrent(UndoStack* stack) { _currentUndoStack = stack; }

	void				pushCommand(UndoModelCommand* command);
	/// The byte cap pass (§11.4): after a push, sum undoBytes() top-down and drop oldest
	/// commands until the stored undo material fits ~250 MB. See the .cpp for the
	/// setUndoLimit trick (QUndoStack cannot remove arbitrary commands).
	void				enforceUndoByteCap();
	void				startMacro(const QString& text = QString());
	void				endMacro(UndoModelCommand* command = nullptr);
	QUndoCommand*		parentCommand()		{ return _parentCommand; }
	
private:

	UndoModelCommand*			_parentCommand			= nullptr;

	static UndoStack*			_currentUndoStack;

};

#endif // UNDOSTACK_H
