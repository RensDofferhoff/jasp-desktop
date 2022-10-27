#ifndef TABNAVIGATIONMODEL_H
#define TABNAVIGATIONMODEL_H

#include <QObject>
#include <QMap>

#include "components/JASP/Widgets/altnavtagbase.h"

class AltNavigationModel : public QObject
{

	Q_OBJECT

public:
	static AltNavigationModel* getInstance();

	bool registerTag(AltNavTagBase* tagObject, AltNavTagBase* parentTagObject = nullptr, QString requestedPostFix = "");
	bool registerTag(AltNavTagBase* tagObject, QString prefix, QString requestedPostFix = "");
	void removeTag(AltNavTagBase* tagObject);

	QString getTagString(AltNavTagBase* tagObject);

	QString getCurrentAltNavInput();
	void updateAltNavInput(QString addedPostFix);
	void resetAltNavInput();

	void setAltNavEnabled(bool value);
	bool isAltNavEnabled();

	bool eventFilter(QObject* object, QEvent* event);

	//singleton stuff
	AltNavigationModel(AltNavigationModel& other) = delete;
	void operator=(const AltNavigationModel&) = delete;

signals:
	void altNavInputChanged();
	void altNavEnabledChanged();

protected:
	AltNavigationModel(QObject* parent = nullptr);


private:
	void _addTag(AltNavTagBase* tagObject, QString tag);
	void _fillTagTree(QString perfix);
	bool _tagFree (QString tag);

	QString currentInput;
	bool altNavEnabled;
	QMap<AltNavTagBase*, QString> objectTagMap;
	QMap<QString, AltNavTagBase*> tagObjectMap;

	static AltNavigationModel* instance;
	static const QList<QString> postfixOptions;

};

#endif // TABNAVIGATIONMODEL_H
