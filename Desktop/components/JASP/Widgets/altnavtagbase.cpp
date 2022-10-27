#include <QObject>

#include "altnavtagbase.h"
#include "gui/altnavigationmodel.h"


AltNavTagBase::AltNavTagBase(QQuickItem *parent)
	: QQuickItem{parent}
{
	setActiveFocusOnTab(false);

	connect(this,									&AltNavTagBase::destroyed,						this,		&AltNavTagBase::_removeTag			);
	connect(AltNavigationModel::getInstance(),		&AltNavigationModel::altNavInputChanged,		this,		&AltNavTagBase::updateTagString		);
	connect(AltNavigationModel::getInstance(),		&AltNavigationModel::altNavEnabledChanged,		this,		&AltNavTagBase::updateVisibility	);
}


void AltNavTagBase::updateVisibility()
{
	if (_show != AltNavigationModel::getInstance()->isAltNavEnabled()) {
		_show = AltNavigationModel::getInstance()->isAltNavEnabled();
		updateTagString();
		emit showChanged();
	}
}


void AltNavTagBase::updateTagString()
{
	QString totalTag = AltNavigationModel::getInstance()->getTagString(this);
	QString altNavInput = AltNavigationModel::getInstance()->getCurrentAltNavInput();

	//partial match at least
	if (totalTag.length() > 0 && totalTag.length() >= altNavInput.length() && totalTag.first(altNavInput.length()) == altNavInput)
	{
		//check for exact match
		if (altNavInput.length() == totalTag.length())
		{
			emit tagMatch();
			AltNavigationModel::getInstance()->setAltNavEnabled(false);
		}
		else
		{
			_tagString = totalTag.mid(altNavInput.length());
			emit tagStringChanged();
		}
	}

}

bool AltNavTagBase::_registerTag()
{
	return AltNavigationModel::getInstance()->registerTag(this, _requestedPrefix, _requestedPostfix);
}

void AltNavTagBase::_removeTag()
{
	AltNavigationModel::getInstance()->removeTag(this);
}

void AltNavTagBase::componentComplete()
{
	QQuickItem::componentComplete();
	_registerTag();
}
