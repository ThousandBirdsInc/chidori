#!/bin/bash

# Check if Docker is installed
if ! command -v docker &> /dev/null
then
    echo "Docker is not installed. Please install Docker and try again."
    exit 1
fi

# Check if Docker Compose is installed
if ! command -v docker-compose &> /dev/null
then
    echo "Docker Compose is not installed. Please install Docker Compose and try again."
    exit 1
fi

# Check if the --build flag was provided
if [[ $* == *--build* ]]
then
    # Rebuild the Docker images
    docker-compose build
fi

# Start the Docker Compose services and run the tests
docker-compose up --exit-code-from tests

# Tear down the Docker Compose services
docker-compose down