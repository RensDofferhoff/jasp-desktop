#include "commander.h"
#include "analysis/analyses.h"
#include "log.h"

Commander* Commander::_instance = nullptr;

Commander *Commander::getInstance()
{
	if (!_instance)
		_instance = new Commander(nullptr);
	return _instance;
}


Commander::Commander(QObject *parent) : QObject{parent}
{
	connectToTransceiver();
}

void Commander::connectToTransceiver()
{
	Transceiver* ch = Transceiver::getInstance();
	connect(ch, &Transceiver::received, this,  &Commander::incomingMessage);
}

void Commander::incomingMessage(std::shared_ptr<Transceiver::Message> msg)
{
	//parse
	//hacky demo
	if(msg->type != Transceiver::Message::Type::JASPCommand)
		return;
	Log::log() << "!!!" << QString(msg->message).toStdString() << std::endl;
	emit startNewAnalysis("Descriptives", "", "", "jaspDescriptives");

}

