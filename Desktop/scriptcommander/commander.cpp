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
	if(msg->type != Transceiver::Message::Type::JASPCommand)
		return;

	//parse command to json
	Json::Value commandRoot;
	Json::CharReaderBuilder readBuilder;
	const std::unique_ptr<Json::CharReader> reader(readBuilder.newCharReader());
	std::string err;
	if (!reader->parse(msg->message.begin(), msg->message.begin() + msg->message.length(), &commandRoot, &err)) {
		Log::log() << "Could not parse command: " << err << std::endl;
		return;
	}
	if(!commandRoot["command"].isString() || !commandRoot["command_payload"].isObject()) {
		Log::log() << "Could not parse command: Command or payload not present or wrong type" << std::endl;
		return;
	}

	//start some action and optionally create something to writeback
	std::string response;
	Json::Value responsePayload;
	bool writeback = processCommand(commandRoot[""]);


	//optional writeback
	if(writeback)
	{
		Json::Value responseRoot;
		responseRoot["id"] = commandRoot["id"].asInt64();
		responseRoot["response"] = response;
		responseRoot["response_payload"] = responsePayload;
		Json::StreamWriterBuilder writeBuilder;
		const std::string jsonString= Json::writeString(writeBuilder, response);
		Transceiver::getInstance()->send({Transceiver::Message::Type::JASPCommandResponse, jsonString.c_str()});
	}
}

