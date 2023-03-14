#ifndef COMMANDER_H
#define COMMANDER_H

#include "qobject.h"
#include "transceiver.h"

class Commander : public QObject
{
	Q_OBJECT

public:
	Commander(QObject *parent);

//	void stop();
//	void restart();
//	bool active() { return _active; };

	//singleton stuff
	static Commander* getInstance();
	Commander(Commander& other) = delete;
	void operator=(const Commander&) = delete;

signals:
	void startNewAnalysis(QString analysisFunction, QString analysisQML, QString analysisTitle, QString module);

private slots:
	void incomingMessage(std::shared_ptr<Transceiver::Message> msg);

private:
	void connectToTransceiver();
//	void disconnectFromTransceiver();

private:
	static Commander* _instance;

	bool _active = true;
};

#endif
