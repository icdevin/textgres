import psycopg2
import sqlite3
from pydantic import BaseModel, Field
from textual import log

def dict_factory(cursor, row):
    fields = [column[0] for column in cursor.description]
    return {key: value for key, value in zip(fields, row)}

class Connection(BaseModel):
    id: int = Field(default=None)
    name: str = Field(default="")
    host: str = Field(default="localhost")
    port: int = Field(default=5432)
    database: str = Field(default="postgres")
    username: str = Field(default="postgres")
    password: str = Field(default="")

    _conn = None

    def load():
        conn = sqlite3.connect("connections.db")
        conn.row_factory = dict_factory
        c = conn.cursor()
        c.execute(
            """
            CREATE TABLE IF NOT EXISTS connections (
                id INTEGER PRIMARY KEY,
                name TEXT,
                host TEXT,
                port INTEGER,
                database TEXT,
                username TEXT,
                password TEXT
            )
            """
        )
        c.execute("SELECT * FROM connections")
        connections = c.fetchall()
        conn.close()
        return [Connection(**connection) for connection in connections]

    # These methods are used to save and delete the connection from the app
    # and do NOT interact with the database defined in the connection

    def save(self) -> None:
        conn = sqlite3.connect("connections.db")
        conn.row_factory = dict_factory
        c = conn.cursor()
        if not self.id:
            new = c.execute(
                "INSERT INTO connections VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING *",
                (None, self.name, self.host, self.port, self.database, self.username, self.password),
            ).fetchone()
            self.id = new['id']
        else:
            c.execute(
                "UPDATE connections SET name = ?, host = ?, port = ?, database = ?, username = ?, password = ? WHERE id = ?",
                (self.name, self.host, self.port, self.database, self.username, self.password, self.id),
            )
        conn.commit()
        conn.close()

    def delete(self) -> None:
        if self._conn is not None:
            self.disconnect()

        conn = sqlite3.connect("connections.db")
        c = conn.cursor()
        c.execute("DELETE FROM connections WHERE name = ?", (self.name,))
        conn.commit()
        conn.close()

    # These methods are used to interact with the database defined in the
    # connection

    def connect(self):
        if not self._conn:
            log("Connecting '{}'".format(self.name))
            self._conn = psycopg2.connect(
                host=self.host,
                port=self.port,
                dbname=self.database,
                user=self.username,
                password=self.password,
            )

    def disconnect(self) -> None:
        log("Disconnecting '{}'".format(self.name))
        if self._conn:
            self._conn.close()
            self._conn = None

    def query(self, query: str):
        if not self._conn:
            self.connect()

        log("Querying '{}'".format(self.name))
        with self._conn.cursor() as cur:
            cur.execute(query)
            results = cur.fetchall()
            return results

    @property
    def connected(self) -> bool:
        return self._conn is not None
