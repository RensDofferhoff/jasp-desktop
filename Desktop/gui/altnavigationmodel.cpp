#include "altnavigationmodel.h"
#include "log.h"
#include "utilities/qutils.h"

#include <QApplication>
#include <QEvent>
#include <QKeyEvent>

AltNavigationModel* AltNavigationModel::instance = nullptr;
const QList<QString> AltNavigationModel::postfixOptions = {"1", "2", "3", "4", "5", "6", "7", "8", "9", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z"};

AltNavigationModel::AltNavigationModel(QObject* parent) : QObject{parent}
{
	_addTag(nullptr, "");
	altNavEnabled = false;
}

AltNavigationModel* AltNavigationModel::getInstance()
{
	if (instance == nullptr)
		instance = new AltNavigationModel;
	return instance;
}

QString AltNavigationModel::getTagString(AltNavTagBase* tagObject)
{
	auto tag = objectTagMap.find(tagObject);
	if(tag != objectTagMap.end())
		return *tag;
	return "";
}


bool AltNavigationModel::registerTag(AltNavTagBase* tagObject, AltNavTagBase* parentTagObject, QString requestPostfix)
{
	auto res = objectTagMap.find(parentTagObject);
	if (res == objectTagMap.end())
		return false;

	QString parentTag = *res;

	return registerTag(tagObject, parentTag, requestPostfix);
}

bool AltNavigationModel::registerTag(AltNavTagBase* tagObject, QString prefix, QString requestPostfix)
{

	_fillTagTree(prefix);

	//handle postfix preference case
	if (requestPostfix != "")
	{
		if (_tagFree(prefix + requestPostfix))
		{
			_addTag(tagObject, prefix + requestPostfix);
			return true;
		}
		else
		{
			Log::log() << "Tag: " + fq(prefix + requestPostfix) + " is not available. Please request a different postfix" << std::endl;
			return false;
		}
	}

	//no postfix preference case
	for (const QString& postfix : postfixOptions)
	{
		if (_tagFree(prefix + postfix))
		{
			_addTag(tagObject, prefix + postfix);
			return true;
		}
	}

	return false;
}

void AltNavigationModel::removeTag(AltNavTagBase* tagObject)
{
	auto tagIt = objectTagMap.find(tagObject);
	if(tagIt != objectTagMap.end())
	{
		QString tag = *tagIt;
		tagObjectMap.remove(tag);
	}
	objectTagMap.remove(tagObject);
}

void AltNavigationModel::updateAltNavInput(QString addedPostFix)
{
	if(altNavEnabled)
	{
		currentInput += addedPostFix;
		if (!tagObjectMap.contains(currentInput))
			setAltNavEnabled(false);
		emit altNavInputChanged();
	}
}

void AltNavigationModel::resetAltNavInput()
{
	currentInput = "";
	if(altNavEnabled)
		emit altNavInputChanged();
}

QString AltNavigationModel::getCurrentAltNavInput()
{
	return currentInput;
}

void AltNavigationModel::setAltNavEnabled(bool value)
{
	altNavEnabled = value;
	if (!altNavEnabled)
	{
		qApp->removeEventFilter(this);
		resetAltNavInput();
	}
	emit altNavEnabledChanged();
}

bool AltNavigationModel::isAltNavEnabled()
{
	return altNavEnabled;
}

//this filter is only active globally once alt has been pressed and disengages upon ESC/ALT press
bool AltNavigationModel::eventFilter(QObject *object, QEvent *event)
{

	//shortcut is not an input event according to Qt...
	if (event->type() == QEvent::Shortcut)
		return true;
	else if (event->type() == QEvent::MouseButtonPress)
		return true;
	else if (event->type() == QEvent::KeyPress)
	{
		QKeyEvent* keyEvent = static_cast<QKeyEvent *>(event);
		int key = keyEvent->key();
		if (key == Qt::Key_Escape || key == Qt::Key_Alt)
		{
			resetAltNavInput();
			setAltNavEnabled(false); //removes this filter
		}
		else if ((key >= Qt::Key_A && key <= Qt::Key_Z) || (key >= Qt::Key_0 && key <= Qt::Key_9))
			updateAltNavInput(keyEvent->text().toUpper());

		return true;

	}
	return false;
}


void AltNavigationModel::_addTag (AltNavTagBase* tagObject, QString tag)
{
	objectTagMap.insert(tagObject, tag);
	tagObjectMap.insert(tag, tagObject);
}


bool AltNavigationModel::_tagFree (QString tag)
{
	auto it =  tagObjectMap.find(tag);
	return it == tagObjectMap.end() || *it == nullptr;
}

void AltNavigationModel::_fillTagTree (QString prefix)
{
	for (int i = 1; i <= prefix.length(); i++) {
		QString part = prefix.first(i);
		if(!tagObjectMap.contains(part))
			tagObjectMap.insert(part, nullptr);
	}
}

