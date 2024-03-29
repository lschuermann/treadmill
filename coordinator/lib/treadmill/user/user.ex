defmodule Treadmill.User do
  use Ecto.Schema
  import Ecto.Changeset

  def log_event(_event) do
    # nada
  end

  @primary_key {:id, :binary_id, autogenerate: true}
  @foreign_key_type :binary_id
  schema "users" do
    field :name, :string
    field :email, :string

    timestamps(type: :utc_datetime)

    has_many :user_providers, Treadmill.User.UserProvider
  end

  @doc false
  def changeset(user, attrs) do
    user
    |> cast(attrs, [:name, :email])
    |> validate_required([:name, :email])
    # |> unique_constraint(:email)
  end
end

defmodule Treadmill.UserSSHKey do
  use Ecto.Schema
  import Ecto.Changeset

  def log_event(_event) do
    # nada
  end

  @primary_key {:id, :binary_id, autogenerate: true}
  @foreign_key_type :binary_id
  schema "user_ssh_keys" do
    field :enabled, :boolean
    field :type, :string
    field :binary_key, :binary
    field :label, :string
    field :github_ssh_key_id, :integer

    timestamps(type: :utc_datetime)

    belongs_to :user, Treadmill.User
  end
end

defmodule Treadmill.User.UserProvider do
  use Ecto.Schema
  import Ecto.Changeset

  @primary_key false
  @foreign_key_type :binary_id
  schema "user_providers" do
    field :provider, :string
    field :token, :string

    belongs_to :user, Treadmill.User

    timestamps(type: :utc_datetime)
  end

  @doc false
  def changeset(user_provider, attrs) do
    user_provider
    |> cast(attrs, [:user_id, :provider, :token])
    |> validate_required([:user_id, :provider, :token])
    |> unique_constraint([:user_id, :provider])
  end

  @doc false
  def changeset_with_user(user_provider, attrs) do
    user_provider
    |> cast(attrs, [:provider, :token])
    |> cast_assoc(:user, with: &Treadmill.User.changeset/2)
    |> validate_required([:user, :provider, :token])
    |> unique_constraint([:user, :provider])
  end
end

defmodule Treadmill.User.LogEvent do
  use Ecto.Schema

  @primary_key false
  @foreign_key_type :binary_id
  schema "log_event_users" do
    belongs_to :log_event, Treadmill.Log
    belongs_to :user, Treadmill.User

    field :user_visible, :boolean
  end
end
