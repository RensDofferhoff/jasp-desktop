#ifndef COMMANDER_H
#define COMMANDER_H

#include "qobject.h"
#include "transceiver.h"
#include "json/json.h"

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

private slots:
	void incomingMessage(std::shared_ptr<Transceiver::Message> msg);

private:
	void connectToTransceiver();
//	void disconnectFromTransceiver();

	bool processCommand(const std::string& command, const Json::Value& commandPayload, std::string& response, Json::Value responsePayload);

private:
	static Commander* _instance;

	bool _active = true;
};

#endif
