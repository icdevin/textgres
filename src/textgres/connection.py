from pydantic import BaseModel, Field

class Connection(BaseModel):
    name: str = Field(default="Local")
    host: str = Field(default="localhost")
    port: int = Field(default=5432)
    database: str = Field(default="postgres")
    username: str = Field(default="postgres")
    password: str = Field(default="postgres")