#!/bin/bash

cd tracing || exit
wget https://github.com/quickwit-oss/quickwit-datasource/releases/download/v0.2.0/quickwit-quickwit-datasource-0.2.0.zip \
&& mkdir -p grafana-storage/plugins \
&& unzip quickwit-quickwit-datasource-0.2.0.zip -d grafana-storage/plugins
