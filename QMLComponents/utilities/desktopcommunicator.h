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
#ifndef DESKTOPCOMMUNICATOR_H
#define DESKTOPCOMMUNICATOR_H

#include <QObject>
#include <condition_variable>
#include <mutex>
#include <QString>

///This class only exists to allow signal-slot connections to be made between certain classes in Desktop and in QMLComponents.
/// And to easily split that off for R-only
class DesktopCommunicator : public QObject
{
	Q_OBJECT
public:
	explicit DesktopCommunicator(QObject *parent = nullptr);
	
	static DesktopCommunicator * singleton();

	bool useNativeFileDialog();
	bool engineSandbox();
	bool queryEncryptionSettings(bool readingMode = false);
	
signals:
	void queryEncryptionSettingsSignal(bool readingMode);
	void currentJaspThemeChanged();
	void uiScaleChanged();
	void interfaceFontChanged();
	bool useNativeFileDialogSignal(); //< For internal use only, `bool useNativeFileDialog();` is what you want
	bool engineSandboxSignal();
	
public slots:
	void encryptionSettingsQueryComplete(bool submit);
	
private:
	static DesktopCommunicator * _singleton;

	bool _queryCondition = false;
	bool _querySubmitted = false;

	std::mutex _queryLock;

	std::condition_variable _query_cv;
};

#endif // DESKTOPCOMMUNICATOR_H
