#include "commander.h"
#include "analysis/analyses.h"
#include "analysisform.h"
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
	if(!commandRoot["command"].isString() || !commandRoot["command_payload"].isObject() || !commandRoot["id"].asInt64()) {
		Log::log() << "Could not parse command: Command, payload or ID not present or wrong type" << std::endl;
		return;
	}

	//start some action that optionally writesback async if needed
	processCommand(commandRoot["command"].asString(), commandRoot["command_payload"], commandRoot["id"].asInt64());
}

bool Commander::processCommand(const std::string &commandStr, const Json::Value &commandPayload, int64_t commandID)
{
	try 
	{
		JaspCommand command = JaspCommandFromString(commandStr);

		switch(command)
		{
			case JaspCommand::createAnalysis:
				return createAnalysis(commandStr, commandPayload, commandID);
			case JaspCommand::setOptions:
				return setOptions(commandStr, commandPayload, commandID);
			default:
				throw std::runtime_error("Command " + commandStr + " does not exist");
		}
	} 
	catch (const std::exception& e) 
	{
		Log::log() << "command failed: " + std::string(e.what()) << std::endl;
		Json::Value responsePayload;
		responsePayload["reason"] = std::string(e.what());
		writeBack(JaspResponse::failure, responsePayload, commandID);
		return false;
	}
}

void Commander::writeBack(const JaspResponse response, const Json::Value &responsePayload, int64_t commandID)
{
	Json::Value responseRoot;
	responseRoot["id"] = commandID;
	responseRoot["response"] = JaspResponseToString(response);
	responseRoot["response_payload"] = responsePayload;
	Json::StreamWriterBuilder writeBuilder;
	const std::string jsonString = Json::writeString(writeBuilder, responseRoot);
	Transceiver::getInstance()->send({Transceiver::Message::Type::JASPCommandResponse, jsonString.c_str()});

}


bool Commander::createAnalysis(const std::string &command, const Json::Value &commandPayload, int64_t commandID)
{
	//some more parsing of payload
	if(!commandPayload["module"].isString() || !commandPayload["analysis"].isString()) {
		throw std::runtime_error("Module or analysis does not exist");
	}
	std::string module = commandPayload["module"].asString();
	std::string analysisName = commandPayload["analysis"].asString();

	//create analysis
	Json::Value options = commandPayload.get("options", Json::nullValue);
	Analysis* analysis;
	if(options != Json::nullValue)
		analysis = Analyses::analyses()->createAnalysis(module.c_str(), analysisName.c_str(), &options);
	else
		analysis = Analyses::analyses()->createAnalysis(module.c_str(), analysisName.c_str());
	if(!analysis)
	{
		throw std::runtime_error("Could not create analysis " + module + ":" + analysisName);
	}

	std::string title = commandPayload.get("title", analysis->title()).asString();
	analysis->setTitle(title);

	//set up an async response
	auto writebackConnection = std::make_shared<QMetaObject::Connection>();
	auto writebackFunc = [this, commandID, writebackConnection, analysis, options]() mutable {
		if (analysis->status() == Analysis::Complete)
		{
			Json::Value responsePayload;
			responsePayload["result"] = analysis->results();
			responsePayload["analysisID"] = static_cast<int64_t>(analysis->id());
			writeBack(JaspResponse::success, responsePayload, commandID);
			QObject::disconnect(*writebackConnection);
		}
	};
	*writebackConnection = connect(analysis, &Analysis::resultsChangedSignal, this, writebackFunc);
	return true;
}

bool Commander::setOptions(const std::string &command, const Json::Value &commandPayload, int64_t commandID)
{
	//some more parsing of payload
	if(!commandPayload["analysisID"].isUInt64()) {
		throw std::runtime_error("No valid analysis ID provided");
	}
	uint64_t analysisID = commandPayload["analysisID"].asUInt64();


	//do something synchronously
	Analysis* analysis = Analyses::analyses()->get(analysisID);
	if(!analysis)
		throw std::runtime_error("Analysis with ID: " + std::to_string(analysisID) + " does not exist");

	Json::Value options = commandPayload.get("options", Json::nullValue);
	if(options == Json::nullValue)
	{
		throw std::runtime_error("no \"options\" provided");
	}


	//set up an async response
	auto connection = std::make_shared<QMetaObject::Connection>();
	*connection = connect(analysis, &Analysis::resultsChangedSignal, this, [this, commandID, connection, analysis]() {
		if (analysis->status() == Analysis::Complete)
		{
			Json::Value responsePayload;
			responsePayload["result"] = analysis->boundValues();
			writeBack(JaspResponse::success, responsePayload, commandID);
			QObject::disconnect(*connection);
		}
	});

	//set options
	analysis->form()->setOptions(options);
	return true;
}



