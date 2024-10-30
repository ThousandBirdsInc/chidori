# Initalling chidori
FROM ghcr.io/rust-lang/rust:nightly-bookworm

RUN apt update
RUN apt install -y cmake curl
RUN apt install -y python3-dev

RUN curl -LsSf https://astral.sh/uv/install.sh | sh
RUN cargo +nightly install chidori-core@0.2.11 --locked
RUN uv venv
RUN uv pip install "litellm[proxy]"

WORKDIR /usr/src
RUN mkdir logs
COPY example_agent /usr/src/example_agent
COPY init.sh ./init.sh
COPY litellm_config.yaml litellm_config.yaml
RUN chmod +x ./init.sh

ENV OPENAI_API_KEY=OPENAI_API_KEY
ENV ANTHROPIC_API_KEY=ANTHROPIC_API_KEY
ENV CHIDORI_HOME_DIRECTORY=/usr/src

EXPOSE 8000

CMD ["./init.sh"]



# Bundle Stage
#FROM scratch
#COPY --from=builder /usr/local/cargo/bin/chidori-core .
#COPY agent ./example_agent
#COPY init.sh ./init.sh
#RUN ["./init.sh"]
#USER 1000
#CMD ["./deciduously-com", "-a", "0.0.0.0", "-p", "8080"]