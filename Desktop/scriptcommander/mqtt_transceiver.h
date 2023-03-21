#ifndef MQTT_TRANSCEIVER_H
#define MQTT_TRANSCEIVER_H

#include "qobject.h"
#include "transceiver.h"
#include <QtMqtt/QMqttClient>

class MqttTransceiver : public Transceiver
{
	Q_OBJECT

public:
	MqttTransceiver(QObject *parent);

	int send(Message const& msg) override;

private slots:
	void messageReceived(const QByteArray& message, const QMqttTopicName& topic);

private:
	QMqttClient _client;
	QString _clientID;

	const QString hostname = "localhost";
	const int port = 53781;
	const int qos = 2;
	const int keepAliveTime = 60;
	const QMqttClient::ProtocolVersion protocolVersion = QMqttClient::MQTT_5_0;
	const QString commandTopic = "jasp_command";
	const QString commandResponseTopic = "jasp_command_response";
};

#endif
