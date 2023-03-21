#include "commander.h"
#include "analysis/analyses.h"
#include "log.h"
#include "json/json.h"

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

	//parse
	Json::Value root;
	Json::CharReaderBuilder readBuilder;
	const std::unique_ptr<Json::CharReader> reader(readBuilder.newCharReader());
	std::string err;
	if (!reader->parse(msg->message.begin(), msg->message.begin() + msg->message.length(), &root, &err)) {
		Log::log() << "Could not parse message: " << err << std::endl;
		return;
	}

	//start some action
	emit startNewAnalysis("Descriptives", "", "", "jaspDescriptives");

	//tmp test return
	Json::Value response;
	response["id"] = root["id"].asInt64();
	response["response"] = "SUCCESS";
	response["response_payload"] = Json::Value(Json::ValueType::objectValue);
	Json::StreamWriterBuilder writeBuilder;
	const std::string jsonString= Json::writeString(writeBuilder, response);
	Transceiver::getInstance()->send({Transceiver::Message::Type::JASPCommandResponse, jsonString.c_str()});
}

