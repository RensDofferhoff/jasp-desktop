#ifndef TABNAVIGATIONMODEL_H
#define TABNAVIGATIONMODEL_H

#include <QObject>
#include <QMap>

class AltNavigationModel : public QObject
{
	Q_OBJECT

	Q_PROPERTY( bool	altNavEnabled		READ	isAltNavEnabled			WRITE	setAltNavEnabled	NOTIFY		altNavEnabledChanged	)
	Q_PROPERTY( QString	currentAltNavInput	READ	getCurrentAltNavInput								NOTIFY		altNavInputChanged		)

public:
	explicit AltNavigationModel(QObject *parent = nullptr);

	Q_INVOKABLE QString getTagString(QObject* tagObject);
	Q_INVOKABLE bool addTag(QObject* tagObject, QObject* parentTagObject = nullptr, QString requestedPostFix = "");
	Q_INVOKABLE bool addTag(QObject* tagObject, QString prefix, QString requestedPostFix = "");
	Q_INVOKABLE void removeTag(QObject* tagObject);

	QString getCurrentAltNavInput();
	void updateAltNavInput(QString addedPostFix);
	void resetAltNavInput();

	void setAltNavEnabled(bool value);
	bool isAltNavEnabled();


	bool eventFilter(QObject *object, QEvent *event);

signals:
	void altNavInputChanged();
	void altNavEnabledChanged();

private:
	void _addTag(QObject* tagObject, QString tag);
	void _fillTagTree(QString perfix);
	bool _tagFree (QString tag);

	QString currentInput;
	bool altNavEnabled;
	QMap<QObject*, QString> objectTagMap;
	QMap<QString, QObject*> tagObjectMap;

	static const QList<QString> postfixOptions;

};

#endif // TABNAVIGATIONMODEL_H
