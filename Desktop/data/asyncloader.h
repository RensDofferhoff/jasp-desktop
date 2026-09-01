//
// Copyright (C) 2013-2026 University of Amsterdam
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

#define BOOST_BIND_GLOBAL_PLACEHOLDERS
#ifndef ASYNCLOADER_H
#define ASYNCLOADER_H

#include <QObject>
#include <QMutex>
#include <QTimer>

#include "datasetpackage.h"
#include "data/fileevent.h"

#include "osf/onlinedatamanager.h"
#include "timers.h"

struct LoaderException : public std::runtime_error
{
	LoaderException(const std::string & _problemDescription, bool _cancelled = false);

	bool cancelled = false;
};

///
/// Runs the (remaining) file-open work in a different thread from the main event loop.
/// NEO (the excision, Cut 2): importers, exporters and the legacy import branch are gone —
/// this now only registers lane-owned CSV opens; every other route fails with a clear
/// "not supported in NEO yet" message (FileEvent::notSupportedInNeoMsg).
class AsyncLoader : public QObject
{
	Q_OBJECT

public:
	explicit AsyncLoader(QObject *parent = 0);

	void io(FileEvent *event);
	void setOnlineDataManager(OnlineDataManager *odm);

signals:
	void beginLoad(FileEvent*);
	void progress(QString status, int progress);
	///Emitted after a load added a dataset to the workspace. The workspace table model
	///was mutated on the loader thread, so the GUI thread connects to this to refresh it (and any
	///views bound to it, e.g. the dataset tabbuttons) from the correct thread.
	void dataSetsChanged();

private slots:
	void loadTask(FileEvent *event);
	void loadPackage(QString id);

private:
	FileEvent			*	_currentEvent	= nullptr;
	OnlineDataManager	*	_odm			= nullptr;
};

#endif // ASYNCLOADER_H
