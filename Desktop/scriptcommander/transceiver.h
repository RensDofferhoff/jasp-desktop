#ifndef TRANSCEIVER_H
#define TRANSCEIVER_H

#include "qobject.h"

class Transceiver : public QObject
{
	Q_OBJECT

public:
	struct Message
	{
		enum class Type {JASPCommand, JASPCommandResponse};
		Type type = Type::JASPCommandResponse;
		QByteArray message;
	};

public:
	Transceiver(QObject *parent) : QObject{parent} {};

	virtual int send(Message const& msg) = 0;
	bool isReady() { return _ready; };

	//singleton stuff
	static Transceiver* getInstance();
	Transceiver(Transceiver& other) = delete;
	void operator=(const Transceiver&) = delete;

signals:
	void received(std::shared_ptr<Message> msg);
	void ready();

protected:
	bool _ready = false;

private:
	static Transceiver* _instance;

};

#endif
