#ifndef ALTNAVTAGBASE_H
#define ALTNAVTAGBASE_H

#include <QQuickItem>

class AltNavTagBase : public QQuickItem
{
	Q_PROPERTY( bool		show				READ	show														NOTIFY		showChanged				)
	Q_PROPERTY( QString		tagString			READ	getCurrentAltNavTag											NOTIFY		tagStringChanged		)
	Q_PROPERTY( QString		requestPrefix		MEMBER	_requestedPrefix																				)
	Q_PROPERTY( QString		requestPostfix		MEMBER	_requestedPostfix																				)

	Q_OBJECT

public:
	explicit AltNavTagBase(QQuickItem *parent = nullptr);
	~AltNavTagBase();


	bool show() { return _show; };
	QString getCurrentAltNavTag() { return _tagString; };

	void componentComplete() override;

signals:
	void tagMatch();
	void tagStringChanged();
	void showChanged();

public slots:
	void updateTagString();
	void updateVisibility();

private:
	bool _registerTag();
	void _removeTag();

	QString _requestedPrefix = "";
	QString _requestedPostfix = "";

	QString _tagString = "";
	bool _show = false;

};

#endif // ALTNAVTAGBASE_H
