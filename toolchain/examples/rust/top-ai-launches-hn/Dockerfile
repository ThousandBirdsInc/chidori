# Use an official Rust runtime as a parent image
FROM rust:1.54.0 as builder

# Set the working directory in the container to /usr/src/myapp
WORKDIR /usr/src/myapp

# Copy the current directory contents into the container at /usr/src/myapp
COPY . .

# Expose port 9800 for the Chidori server to listen on
EXPOSE 9800

# Run the application (we're not going for build or runtime performance here)
CMD ["cargo run"]
