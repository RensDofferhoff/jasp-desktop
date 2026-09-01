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
#include "desktopcommunicator.h"
#include <QThread>
#include <QGuiApplication>
#include <cassert>
#include <QMetaMethod>

DesktopCommunicator * DesktopCommunicator::_singleton = nullptr;

DesktopCommunicator::DesktopCommunicator(QObject *parent)
	: QObject{parent}
{
	assert(!_singleton);
	
	_singleton = this;
}

DesktopCommunicator *DesktopCommunicator::singleton()
{
	if(!_singleton)
		new DesktopCommunicator();

	return _singleton;
}

bool DesktopCommunicator::useNativeFileDialog()
{
#ifdef BUILDING_JASP
	return emit useNativeFileDialogSignal();
#else
	return true;
#endif
}

bool DesktopCommunicator::engineSandbox()
{
#ifdef BUILDING_JASP
	return emit engineSandboxSignal();
#else
	return false;
#endif
}

bool DesktopCommunicator::queryEncryptionSettings(bool readingMode)
{
#ifdef BUILDING_JASP
	if(QThread::currentThread() == qApp->thread()) {
		int x = 1;
	}

	_queryCondition = false;
	_querySubmitted = false;
	std::unique_lock<std::mutex> lock(_queryLock);
	emit queryEncryptionSettingsSignal(readingMode);
	_query_cv.wait(lock, [&] { return _queryCondition; });

	return _querySubmitted;
#else
	return true;
#endif
}

// DesktopCommunicator::askCsvDelimiter is gone with the importers (the excision, Cut 2):
// the data lane sniffs the CSV delimiter itself, so no dialog is ever needed.

void DesktopCommunicator::encryptionSettingsQueryComplete(bool submit)
{
	std::lock_guard<std::mutex> lock(_queryLock);
	_queryCondition = true;
	_querySubmitted = submit;
	_query_cv.notify_one();
}
