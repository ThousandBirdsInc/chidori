# Use an official Node.js runtime as a parent image
FROM node:20

# Set the working directory in the container to /app
WORKDIR /app

# Copy package.json and package-lock.json into the /app directory
COPY package*.json ./

# Install dependencies
RUN ldd --version && npm install --only=production


# Copy the current directory contents into the container at /app
COPY . .

# Expose port 9800 for the Chidori server to listen on
EXPOSE 9800

# Run the application
CMD [ "node", "index.js" ]
