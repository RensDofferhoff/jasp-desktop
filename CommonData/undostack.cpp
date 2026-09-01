#include "log.h"
#include "undostack.h"
#include "workspace.h"
#include <cassert>
#include <algorithm>

UndoStack* UndoStack::_currentUndoStack = nullptr;

UndoStack::UndoStack(QObject* parent) : QUndoStack(parent)
{
	connect(this, &QUndoStack::indexChanged, []() { if(Workspace::singleton()) Workspace::singleton()->somethingModified(); });
}

void UndoStack::pushCommand(UndoModelCommand *command)
{
	if (!_parentCommand) // Push to the stack only when no macro is started: in this case the command is autmatically added to the _parentCommand
	{
		push(command);
		enforceUndoByteCap();
	}
}

void UndoStack::enforceUndoByteCap()
{
	// The byte-based undo cap (data-edit-design §11.4; the 2026-08-31 leaning): drop
	// OLDEST-first once the stored undo material exceeds ~250 MB. QUndoStack cannot remove
	// arbitrary commands, but setUndoLimit(n) discards from the bottom — so compute the
	// largest top-down run whose cumulative undoBytes() fits and set that as the limit.
	// (A limit of 0 would DISABLE capping, so a fully-trimming edge clamps to 1.)
	constexpr size_t kUndoByteCap = 250ull * 1024 * 1024;

	size_t		sum		= 0;
	const int	total	= static_cast<int>(count());
	int			fits	= 0;
	for (int i = total - 1; i >= 0; --i)
	{
		const size_t bytes = static_cast<const UndoModelCommand*>(command(i))->undoBytes();
		if (sum + bytes > kUndoByteCap)
			break;
		sum += bytes;
		++fits;
	}
	if (fits < total)
	{
		Log::log() << "UndoStack: undo material " << sum / (1024 * 1024) << " MB at " << fits
				   << " command(s) — dropping the oldest " << (total - fits)
				   << " (byte cap " << (kUndoByteCap / (1024 * 1024)) << " MB)" << std::endl;
		setUndoLimit(std::max(fits, 1));
		setUndoLimit(0); // re-disable the count-limit — the cap is byte-driven, not count-driven
	}
}

void UndoStack::startMacro(const QString &text)
{
	if (_parentCommand)
	{
		Log::log() << "Macro started though last one is not finished!" << std::endl; //I think this should be an assert...
		delete _parentCommand; //Which it never was, so instead of leaking the unfinished macro we drop it here.
	}
	
	_parentCommand = new UndoModelCommand();
	
	if (!text.isEmpty())
		_parentCommand->setText(text);
}

void UndoStack::endMacro(UndoModelCommand *command)
{
	if(!_parentCommand)
	{
		if (command)
		{
			push(command);
			enforceUndoByteCap();
		}
		return;
	}
	
	if (command && _parentCommand->text().isEmpty())
		_parentCommand->setText(command->text());
	
	
	if (_parentCommand)
	{
		push(_parentCommand);
		// A macro's undo material lives in its CHILDREN: walk them (the wrapper itself
		// carries none). Over-cap macros are logged, not trimmed — a single gesture's honest
		// minimum (§5) is kept even when large.
		size_t sum = 0;
		for (int i = 0; i < _parentCommand->childCount(); ++i)
			sum += static_cast<const UndoModelCommand*>(_parentCommand->child(i))->undoBytes();
		if (sum > 250ull * 1024 * 1024)
			Log::log() << "UndoStack: macro pushes " << sum / (1024 * 1024) << " MB of undo material (over the 250 MB cap)" << std::endl;
	}

	_parentCommand = nullptr;
}

UndoModelCommand::UndoModelCommand(DataSet * data)
	: QUndoCommand(UndoStack::singleton()->parentCommand())
{
	_dataSetID = data ? data->id() : -1;
}

bool UndoModelCommand::dataSetStillExists() const
{
	return dataSet();
}

DataSet *UndoModelCommand::dataSet() const
{
	assert(_dataSetID > -1);
	
	DataSet * data = Workspace::singleton()->dataSetById(_dataSetID);
	
	return data;
}
