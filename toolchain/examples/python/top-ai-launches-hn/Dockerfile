# Use an official Python runtime as a parent image
FROM python:3.10

# Set environment variables
ENV PYTHNDONTWRITEBYTECODE 1
ENV PYTHONUNBUFFERED 1
ENV POETRY_VERSION=1.1.7

# Set the working directory in the container to /app
WORKDIR /app

# Install Poetry
RUN pip install "poetry==$POETRY_VERSION"

# Copy the pyproject.toml (and optionally poetry.lock) file into the /app directory
COPY pyproject.toml poetry.lock* /app/

RUN curl https://sh.rustup.rs -sSf | bash -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"


# Install dependencies
RUN poetry config virtualenvs.create false \
  && poetry install --no-interaction --no-ansi

# Copy the current directory contents into the container at /app
COPY . /app/

# Expose port 9800 for the Chidori server to listen on
EXPOSE 9800

# Run the application
CMD ["python", "top-ai-launches-hn.py"]
