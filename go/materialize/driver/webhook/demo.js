var http = require('http');

const delay = 2000; // Delay to model network latency and API back-pressure.
const port = 9000; // Port we'll listen on.

http.createServer(function (request, response) {
  request.on("data", function (data) {
      // Dump POST body to stdout.
      console.log(data.toString('utf8'));
  })
  request.on("end", function() {
    // On reading client close, send 'OK' after |delay| has elapsed.
    setTimeout(function() {
        response.write("OK");
        response.end();
    }, delay);
  })
}).listen(port);