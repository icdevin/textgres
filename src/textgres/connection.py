import sqlite3
from pydantic import BaseModel, Field

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

    def save(self) -> None:
        conn = sqlite3.connect("connections.db")
        c = conn.cursor()
        if self.id:
            c.execute(
                "INSERT INTO connections VALUES (?, ?, ?, ?, ?, ?, ?)",
                (None, self.name, self.host, self.port, self.database, self.username, self.password),
            )
        else:
            c.execute(
                "UPDATE connections SET name = ?, host = ?, port = ?, database = ?, username = ?, password = ? WHERE id = ?",
                (self.name, self.host, self.port, self.database, self.username, self.password, self.id),
            )
        conn.commit()
        conn.close()

    def delete(self) -> None:
        conn = sqlite3.connect("connections.db")
        c = conn.cursor()
        c.execute("DELETE FROM connections WHERE name = ?", (self.name,))
        conn.commit()
        conn.close()