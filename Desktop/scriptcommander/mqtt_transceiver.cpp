#include "mqtt_transceiver.h"
#include <QRandomGenerator>
#include "log.h"

MqttTransceiver::MqttTransceiver(QObject *parent) : Transceiver{parent}
{
	//Todo launch broker?

	//Todo retry on various ports
	_clientID= "jasp-desktop_" + QString::number(QRandomGenerator::global()->generate64());
	_client.setClientId(_clientID);
	_client.setPort(port);
	_client.setHostname(hostname);
	_client.setProtocolVersion(protocolVersion);
	_client.setKeepAlive(keepAliveTime);
	_client.setAutoKeepAlive(true);

	_client.setWillTopic(commandResponseTopic);
	_client.setWillQoS(qos);
	_client.setWillMessage((_clientID +  " died").toUtf8());
	_client.connectToHost();

	connect(&_client, &QMqttClient::connected, this, [&]() {
		_client.subscribe(QMqttTopicFilter(commandTopic), qos);
		_ready = true;
		emit Transceiver::ready();
	});

	connect(&_client, &QMqttClient::disconnected, this, [&]() {
		_ready = false;
		_client.connectToHost();
	});

	connect(&_client, &QMqttClient::messageReceived, this, &MqttTransceiver::messageReceived);

}

int MqttTransceiver::send(const Message& msg)
{
	if (msg.type == Message::Type::JASPCommandResponse)
	{
		_client.publish(commandResponseTopic, msg.message, qos);
	}
	else
		return 0;

	return 1;
}

void MqttTransceiver::messageReceived(const QByteArray &message, const QMqttTopicName &topic)
{
	if(topic.name() == commandTopic)
	{
		Message* msg = new Message{Message::Type::JASPCommand, message};
		Log::log() << "!!!" << QString(message).toStdString() << std::endl;
		emit Transceiver::received(std::shared_ptr<Message>(msg));
	}
}
